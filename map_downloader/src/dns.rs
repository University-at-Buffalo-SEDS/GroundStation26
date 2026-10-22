//! Share DNS work across tile connections without changing transfer concurrency.
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tokio::{
    sync::Mutex as AsyncMutex,
    time::{Duration, Instant, timeout},
};

const FRESH_FOR: Duration = Duration::from_secs(60);
const STALE_FOR: Duration = Duration::from_secs(300);
const RETRY_AFTER: Duration = Duration::from_secs(1);
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HOSTS: usize = 64;

type HostCache = Arc<AsyncMutex<HostState>>;

#[derive(Default)]
struct HostState {
    addresses: Vec<SocketAddr>,
    resolved_at: Option<Instant>,
    retry_at: Option<Instant>,
    error: String,
}

impl HostState {
    fn reusable(&self, now: Instant, max_age: Duration) -> bool {
        !self.addresses.is_empty()
            && self
                .resolved_at
                .is_some_and(|at| now.duration_since(at) < max_age)
    }

    fn addresses(&self) -> Addrs {
        Box::new(self.addresses.clone().into_iter())
    }
}

pub struct CachedResolver {
    inner: Arc<dyn Resolve>,
    hosts: Mutex<HashMap<String, HostCache>>,
}

impl CachedResolver {
    pub fn system() -> Self {
        Self::new(Arc::new(SystemResolver))
    }

    fn new(inner: Arc<dyn Resolve>) -> Self {
        Self {
            inner,
            hosts: Mutex::new(HashMap::new()),
        }
    }
}

impl Resolve for CachedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let state = {
            let mut hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(state) = hosts.get(name.as_str()) {
                state.clone()
            } else {
                let state = Arc::new(AsyncMutex::new(HostState::default()));
                // Bound memory even if a remote service redirects across many hosts.
                // Extra hosts still resolve normally, without persistent caching.
                if hosts.len() < MAX_HOSTS {
                    hosts.insert(name.as_str().to_owned(), state.clone());
                }
                state
            }
        };
        let inner = self.inner.clone();
        Box::pin(async move {
            // Hold only this host's lock across resolution: concurrent connections
            // reuse its result instead of launching duplicate system DNS requests.
            let mut state = state.lock().await;
            let now = Instant::now();
            if state.reusable(now, FRESH_FOR) {
                return Ok(state.addresses());
            }
            if !state.retry_at.is_some_and(|at| now < at) {
                let result = match timeout(LOOKUP_TIMEOUT, inner.resolve(name)).await {
                    Ok(result) => result.map(|addresses| addresses.collect::<Vec<_>>()),
                    Err(_) => Err(Box::new(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "DNS lookup timed out",
                    ))
                        as Box<dyn std::error::Error + Send + Sync>),
                };
                match result {
                    Ok(addresses) if !addresses.is_empty() => {
                        state.addresses = addresses;
                        state.resolved_at = Some(Instant::now());
                        state.retry_at = None;
                        state.error.clear();
                        return Ok(state.addresses());
                    }
                    Ok(_) => state.error = "DNS lookup returned no addresses".into(),
                    Err(error) => state.error = error.to_string(),
                }
                state.retry_at = Some(Instant::now() + RETRY_AFTER);
            }
            // A brief DNS outage must not discard a recently valid address.
            // The HTTPS connector still verifies the requested hostname, and
            // stale results expire rather than pinning an address indefinitely.
            if state.reusable(Instant::now(), STALE_FOR) {
                return Ok(state.addresses());
            }
            Err(Box::new(io::Error::other(state.error.clone())) as _)
        })
    }
}

struct SystemResolver;

impl Resolve for SystemResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((name.as_str(), 0)).await?;
            Ok(Box::new(addresses.collect::<Vec<_>>().into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct ScriptedResolver {
        calls: Arc<AtomicUsize>,
        answers: Mutex<VecDeque<io::Result<Vec<SocketAddr>>>>,
    }

    impl Resolve for ScriptedResolver {
        fn resolve(&self, _: Name) -> Resolving {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let result = self
                .answers
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected lookup");
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Ok(Box::new(result?.into_iter()) as Addrs)
            })
        }
    }

    fn setup(answers: Vec<io::Result<Vec<SocketAddr>>>) -> (CachedResolver, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let resolver = CachedResolver::new(Arc::new(ScriptedResolver {
            calls: calls.clone(),
            answers: Mutex::new(answers.into()),
        }));
        (resolver, calls)
    }

    fn address(octet: u8) -> Vec<SocketAddr> {
        vec![SocketAddr::from(([192, 0, 2, octet], 0))]
    }

    async fn lookup(resolver: &CachedResolver) -> io::Result<Vec<SocketAddr>> {
        resolver
            .resolve("tiles.example".parse().unwrap())
            .await
            .map(Iterator::collect)
            .map_err(io::Error::other)
    }

    async fn age(resolver: &CachedResolver, duration: Duration) {
        let state = resolver.hosts.lock().unwrap()["tiles.example"].clone();
        let mut state = state.lock().await;
        state.resolved_at = Some(Instant::now() - duration);
        state.retry_at = None;
    }

    #[tokio::test]
    async fn concurrent_connections_share_one_lookup_and_cached_result() {
        let (resolver, calls) = setup(vec![Ok(address(1))]);
        let results = futures::future::join_all((0..1024).map(|_| lookup(&resolver))).await;
        assert!(results.into_iter().all(|r| r.unwrap() == address(1)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn expired_cache_survives_transient_dns_failure_then_refreshes() {
        let (resolver, calls) = setup(vec![
            Ok(address(1)),
            Err(io::Error::from_raw_os_error(22)),
            Ok(address(2)),
        ]);
        assert_eq!(lookup(&resolver).await.unwrap(), address(1));
        age(&resolver, FRESH_FOR + Duration::from_secs(1)).await;
        for _ in 0..100 {
            assert_eq!(lookup(&resolver).await.unwrap(), address(1));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        age(&resolver, FRESH_FOR + Duration::from_secs(2)).await;
        assert_eq!(lookup(&resolver).await.unwrap(), address(2));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn failure_without_valid_cache_is_bounded_and_can_recover() {
        let (resolver, calls) = setup(vec![Err(io::Error::from_raw_os_error(22)), Ok(address(2))]);
        for _ in 0..100 {
            assert!(
                lookup(&resolver)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("22")
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let state = resolver.hosts.lock().unwrap()["tiles.example"].clone();
        state.lock().await.retry_at = None;
        assert_eq!(lookup(&resolver).await.unwrap(), address(2));
    }

    #[tokio::test]
    async fn stale_addresses_expire_and_are_not_used_forever() {
        let (resolver, calls) = setup(vec![Ok(address(1)), Err(io::Error::from_raw_os_error(22))]);
        lookup(&resolver).await.unwrap();
        age(&resolver, STALE_FOR + Duration::from_secs(1)).await;
        assert!(lookup(&resolver).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
