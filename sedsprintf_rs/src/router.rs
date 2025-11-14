use crate::{
    config::{DataEndpoint, DataType, DEVICE_IDENTIFIER, MAX_HANDLER_RETRIES}, get_needed_message_size, impl_letype_num,
    lock::RouterMutex,
    message_meta, serialize,
    telemetry_packet::TelemetryPacket,
    MessageElementCount, TelemetryError,
    TelemetryResult,
};
use alloc::{boxed::Box, collections::VecDeque, format, sync::Arc, vec, vec::Vec};

#[cfg(all(not(feature = "std"), target_os = "none"))]
use crate::seds_error_msg;


pub enum RxItem {
    Packet(TelemetryPacket),
    Serialized(Arc<[u8]>),
}

// -------------------- endpoint + board config --------------------
// Make handlers usable across tasks
pub enum EndpointHandlerFn {
    Packet(Arc<dyn Fn(&TelemetryPacket) -> TelemetryResult<()> + Send + Sync>),
    Serialized(Arc<dyn Fn(&[u8]) -> TelemetryResult<()> + Send + Sync>),
}

pub struct EndpointHandler {
    endpoint: DataEndpoint,
    handler: EndpointHandlerFn,
}

impl EndpointHandler {
    /// Create a new endpoint handler for `TelemetryPacket` callbacks.
    /// The provided function `f` will be called whenever a packet is
    /// received for the specified endpoint.
    /// The function must accept a reference to a `TelemetryPacket` as input and return a `SedsResult` as output.
    pub fn new_packet_handler<F>(endpoint: DataEndpoint, f: F) -> Self
    where
        F: Fn(&TelemetryPacket) -> TelemetryResult<()> + Send + Sync + 'static,
    {
        Self {
            endpoint,
            handler: EndpointHandlerFn::Packet(Arc::new(f)),
        }
    }
    /// Create a new endpoint handler for serialized byte slice callbacks.
    /// The provided function `f` will be called whenever a packet is
    /// received for the specified endpoint.
    /// The function must accept a byte slice (`&[u8]`) as input and return a `SedsResult` as output.
    pub fn new_serialized_handler<F>(endpoint: DataEndpoint, f: F) -> Self
    where
        F: Fn(&[u8]) -> TelemetryResult<()> + Send + Sync + 'static,
    {
        Self {
            endpoint,
            handler: EndpointHandlerFn::Serialized(Arc::new(f)),
        }
    }

    /// Return the endpoint that the handler is registered for.
    pub fn get_endpoint(&self) -> DataEndpoint {
        self.endpoint
    }
    /// Return a clone of the handler function.
    pub fn get_handler(&self) -> &EndpointHandlerFn {
        &self.handler
    }
}

pub trait Clock {
    /// Return the current time in milliseconds.
    fn now_ms(&self) -> u64;
}

impl<T: Fn() -> u64> Clock for T {
    #[inline]
    /// Return the current time in milliseconds.
    fn now_ms(&self) -> u64 {
        self()
    }
}

#[derive(Default)]
pub struct BoardConfig {
    /// Handlers for local endpoints.
    handlers: Arc<[EndpointHandler]>,
}

impl BoardConfig {
    /// Create a new board configuration with the specified endpoint handlers.
    pub fn new<H>(handlers: H) -> Self
    where
        H: Into<Arc<[EndpointHandler]>>,
    {
        Self {
            handlers: handlers.into(),
        }
    }

    #[inline]
    /// Check if the specified endpoint is local to this board.
    fn is_local_endpoint(&self, ep: DataEndpoint) -> bool {
        self.handlers.iter().any(|h| h.endpoint == ep)
    }
}

// -------------------- generic little-endian serialization --------------------

pub trait LeBytes: Copy + Sized {
    const WIDTH: usize;
    fn write_le(self, out: &mut [u8]);
    fn from_le_slice(bytes: &[u8]) -> Self;
}

impl_letype_num!(u8, 1);
impl_letype_num!(u16, 2);
impl_letype_num!(u32, 4);
impl_letype_num!(u64, 8);
impl_letype_num!(u128, 16);
impl_letype_num!(i8, 1);
impl_letype_num!(i16, 2);
impl_letype_num!(i32, 4);
impl_letype_num!(i64, 8);
impl_letype_num!(i128, 16);
impl_letype_num!(f32, 4);
impl_letype_num!(f64, 8);
/// Encode a slice of `LeBytes` into a contiguous little-endian byte array.
pub(crate) fn encode_slice_le<T: LeBytes>(data: &[T]) -> Arc<[u8]> {
    let total = data.len() * T::WIDTH;
    let mut buf = Vec::with_capacity(total);
    unsafe { buf.set_len(total) };
    for (i, v) in data.iter().copied().enumerate() {
        let start = i * T::WIDTH;
        v.write_le(&mut buf[start..start + T::WIDTH]);
    }
    Arc::from(buf)
}

/// Build an error payload for `TelemetryError` packets, respecting the
/// static/dynamic sizing rules from `message_meta`.
fn make_error_payload(msg: &str) -> Arc<[u8]> {
    let meta = message_meta(DataType::TelemetryError);
    match meta.element_count {
        MessageElementCount::Static(_) => {
            let max = get_needed_message_size(DataType::TelemetryError);
            let bytes = msg.as_bytes();
            let n = core::cmp::min(max, bytes.len());
            let mut buf = vec![0u8; max];
            if n > 0 {
                buf[..n].copy_from_slice(&bytes[..n]);
            }
            Arc::from(buf)
        }
        MessageElementCount::Dynamic => Arc::from(msg.as_bytes()),
    }
}
/// Generic raw logger function used by Router::log and Router::log_queue.
/// Builds a TelemetryPacket from the provided data slice and passes it to the
/// provided transmission function.
/// # Arguments
/// * `sender` - The sender identifier for the packet.
/// * `ty` - The DataType of the packet.
/// * `data` - The slice of data to log.
/// * `timestamp` - The timestamp for the packet.
/// * `tx_function` - A function that takes a TelemetryPacket and transmits it.
/// # Returns
/// A TelemetryResult indicating success or failure.
/// # Type Parameters
/// * `T` - The type of the data elements (must implement LeBytes).
/// * `F` - The type of the transmission function.
/// # Constraints
/// * `T` must implement LeBytes.
/// * `F` must be a function that takes a TelemetryPacket and returns a TelemetryResult.
fn log_raw<T, F>(
    sender: &'static str,
    ty: DataType,
    data: &[T],
    timestamp: u64,
    mut tx_function: F,
) -> TelemetryResult<()>
where
    T: LeBytes,
    F: FnMut(TelemetryPacket) -> TelemetryResult<()>,
{
    let meta = message_meta(ty);
    let got = data.len() * T::WIDTH;

    match meta.element_count {
        MessageElementCount::Static(_) => {
            if got != get_needed_message_size(ty) {
                return Err(TelemetryError::SizeMismatch {
                    expected: get_needed_message_size(ty),
                    got,
                });
            }
        }
        MessageElementCount::Dynamic => {
            // For dynamic numeric/bool payloads, require length to be a multiple of element width.
            if got % T::WIDTH != 0 {
                return Err(TelemetryError::SizeMismatch {
                    // Express the expectation per element width for clarity.
                    expected: T::WIDTH,
                    got,
                });
            }
        }
    }

    let payload = encode_slice_le(data);

    let pkt = TelemetryPacket::new(ty, &meta.endpoints, sender, timestamp, payload)?;

    tx_function(pkt)
}
/// Fallback stdout printing for error messages when no local endpoints exist.
/// In `no_std` builds this is a no-op.
fn fallback_stdout(msg: &str) {
    #[cfg(feature = "std")]
    {
        eprintln!("{}", msg);
    }
    #[cfg(not(feature = "std"))]
    {
        let message = format!("{}\n", msg);
        unsafe {
            seds_error_msg(message.as_ptr(), message.len());
        }
    }
}

// -------------------- Router --------------------

/// Internal state of the Router, protected by a mutex.
/// Holds the RX and TX queues.
/// Used to allow safe concurrent access from multiple tasks.
/// This struct is private to the Router implementation.
struct RouterInner {
    received_queue: VecDeque<RxItem>,
    transmit_queue: VecDeque<TelemetryPacket>,
}

/// Telemetry Router for handling incoming and outgoing telemetry packets.
/// Supports queuing, processing, and dispatching to local endpoint handlers.
/// Thread-safe via internal locking.
pub struct Router {
    sender: &'static str,
    // make TX usable across tasks
    transmit: Option<Box<dyn Fn(&[u8]) -> TelemetryResult<()> + Send + Sync>>,
    cfg: BoardConfig,
    state: RouterMutex<RouterInner>,
    clock: Box<dyn Clock + Send + Sync>,
}

impl Router {
    /// Create a new Router with the specified transmit function, board configuration, and clock.
    /// The transmit function is called to send serialized telemetry packets.
    /// The board configuration defines local endpoint handlers.
    /// The clock provides the current time in milliseconds.
    /// # Arguments
    /// * `transmit` - Optional transmit function for sending serialized packets.
    /// * `cfg` - Board configuration with local endpoint handlers.
    /// * `clock` - Clock implementation for timestamping.
    /// # Returns
    /// A new Router instance.
    /// # Type Parameters
    /// * `Tx` - Type of the transmit function.
    /// # Constraints
    /// * `Tx` must be a function that takes a byte slice and returns a Tele
    /// metryResult.
    /// * `Tx` must be `Send` and `Sync` to allow safe concurrent access.
    /// * `Tx` must have a static lifetime.
    /// # Example
    /// ```text
    /// struct MyClock;
    /// impl Clock for MyClock {
    ///     fn now_ms(&self) -> u64 {
    ///         // Implement your clock logic here
    ///         0
    ///     }
    /// }
    ///
    /// let board_cfg = BoardConfig::default();
    /// let clock = Box::new(MyClock);
    /// let router = Router::new(None, board_cfg, clock);
    /// ```
    pub fn new<Tx>(
        transmit: Option<Tx>,
        cfg: BoardConfig,
        clock: Box<dyn Clock + Send + Sync>,
    ) -> Self
    where
        Tx: Fn(&[u8]) -> TelemetryResult<()> + Send + Sync + 'static,
    {
        Self {
            sender: DEVICE_IDENTIFIER,
            transmit: transmit.map(|t| Box::new(t) as _),
            cfg,
            state: RouterMutex::new(RouterInner {
                received_queue: VecDeque::with_capacity(16),
                transmit_queue: VecDeque::with_capacity(16),
            }),
            clock,
        }
    }
    /// Error helper when we have a full TelemetryPacket.
    /// Sends a TelemetryError packet to all local endpoints except the failed one (if any).
    /// If no local endpoints remain, falls back to stdout printing.
    /// # Arguments
    /// * `pkt` - The original TelemetryPacket that caused the error.
    /// * `dest` - The endpoint that failed (if any).
    /// * `e` - The TelemetryError that occurred.
    /// # Returns
    /// A TelemetryResult indicating success or failure of sending the error packet.
    fn handle_callback_error(
        &self,
        pkt: &TelemetryPacket,
        dest: Option<DataEndpoint>,
        e: TelemetryError,
    ) -> TelemetryResult<()> {
        let error_msg = match dest {
            Some(failed_local) => format!(
                "Handler for endpoint {:?} failed on device {:?}: {:?}",
                failed_local, DEVICE_IDENTIFIER, e
            ),
            None => format!(
                "TX Handler failed on device {:?}: {:?}",
                DEVICE_IDENTIFIER, e
            ),
        };

        let mut locals: Vec<DataEndpoint> = pkt
            .endpoints()
            .iter()
            .copied()
            .filter(|&ep| self.cfg.is_local_endpoint(ep))
            .collect();
        locals.sort_unstable();
        locals.dedup();

        if let Some(failed_local) = dest {
            locals.retain(|&ep| ep != failed_local);
        }

        if dest.is_none() && locals.is_empty() {
            fallback_stdout(&error_msg);
            return Ok(());
        } else if dest.is_some() && locals.is_empty() {
            return Ok(());
        }

        let payload = make_error_payload(&error_msg);

        let error_pkt = TelemetryPacket::new(
            DataType::TelemetryError,
            &locals,
            self.sender,
            self.clock.now_ms(),
            payload,
        )?;

        self.transmit_message(&error_pkt)
    }

    // ---------- PUBLIC API: now all &self (thread-safe via internal locking) ----------
    /// Process all packets in the transmit queue.
    /// This function will dequeue and transmit all packets in the transmit queue
    /// until the queue is empty.
    /// # Returns
    /// A TelemetryResult indicating success or failure.
    pub fn process_tx_queue(&self) -> TelemetryResult<()> {
        loop {
            let pkt_opt = {
                // Pop exactly one under the lock, then release it.
                let mut st = self.state.lock();
                st.transmit_queue.pop_front()
            };
            let Some(pkt) = pkt_opt else { break };
            self.transmit_message(&pkt)?; // No lock held while calling user code
        }
        Ok(())
    }

    /// Process all packets in the receive queue.
    /// This function will dequeue and process all packets in the receive queue
    /// until the queue is empty.
    /// # Returns
    /// A TelemetryResult indicating success or failure.
    pub fn process_all_queues(&self) -> TelemetryResult<()> {
        self.process_tx_queue()?;
        self.process_rx_queue()?;
        Ok(())
    }

    /// Clear both the transmit and receive queues.
    /// This function will remove all packets from both queues.
    /// No processing is done on the packets.
    pub fn clear_queues(&self) {
        let mut st = self.state.lock();
        st.transmit_queue.clear();
        st.received_queue.clear();
    }

    /// Clear only the receive queue.
    /// This function will remove all packets from the receive queue.
    /// No processing is done on the packets.
    pub fn clear_rx_queue(&self) {
        let mut st = self.state.lock();
        st.received_queue.clear();
    }

    /// Clear only the transmit queue.
    /// This function will remove all packets from the transmit queue.
    /// No processing is done on the packets.
    pub fn clear_tx_queue(&self) {
        let mut st = self.state.lock();
        st.transmit_queue.clear();
    }

    /// Process packets in the transmit queue for up to `timeout_ms` milliseconds.
    /// This function will dequeue and transmit packets in the transmit queue
    /// until the queue is empty or the timeout is reached.
    /// # Arguments
    /// * `timeout_ms` - The maximum time in milliseconds to process packets.
    /// # Returns
    /// A TelemetryResult indicating success or failure.
    pub fn process_tx_queue_with_timeout(&self, timeout_ms: u32) -> TelemetryResult<()> {
        let start = self.clock.now_ms();
        loop {
            let pkt_opt = {
                let mut st = self.state.lock();
                st.transmit_queue.pop_front()
            };
            let Some(pkt) = pkt_opt else { break };
            self.transmit_message(&pkt)?;
            if self.clock.now_ms().wrapping_sub(start) >= timeout_ms as u64 {
                break;
            }
        }
        Ok(())
    }

    /// Process a single item from the receive queue.
    /// # Arguments
    /// * `item` - The RxItem to process.
    /// # Returns
    /// A TelemetryResult indicating success or failure.
    fn process_rx_queue_item(&self, item: RxItem) -> TelemetryResult<()> {
        self.receive_item(&item)
    }

    /// Process packets in the receive queue for up to `timeout_ms` milliseconds.
    /// This function will dequeue and process packets in the receive queue
    /// until the queue is empty or the timeout is reached.
    /// # Arguments
    /// * `timeout_ms` - The maximum time in milliseconds to process packets.
    /// # Returns
    /// A TelemetryResult indicating success or failure.
    pub fn process_rx_queue_with_timeout(&self, timeout_ms: u32) -> TelemetryResult<()> {
        let start = self.clock.now_ms();
        loop {
            let item_opt = {
                let mut st = self.state.lock();
                st.received_queue.pop_front()
            };
            let Some(item) = item_opt else { break };
            self.process_rx_queue_item(item)?;
            if self.clock.now_ms().wrapping_sub(start) >= timeout_ms as u64 {
                break;
            }
        }
        Ok(())
    }

    /// Process all packets in both the transmit and receive queues
    /// for up to `timeout_ms` milliseconds.
    /// This function will dequeue and process packets in both queues
    /// until both queues are empty or the timeout is reached.
    /// # Arguments
    /// * `timeout_ms` - The maximum time in milliseconds to process packets.
    /// # Returns
    /// A TelemetryResult indicating success or failure.
    /// If `timeout_ms` is zero, the function will drain both queues fully.
    pub fn process_all_queues_with_timeout(&self, timeout_ms: u32) -> TelemetryResult<()> {
        let drain_fully = timeout_ms == 0;
        let start = if drain_fully { 0 } else { self.clock.now_ms() };

        loop {
            let mut did_any = false;

            // TX first
            if let Some(pkt) = {
                let mut st = self.state.lock();
                st.transmit_queue.pop_front()
            } {
                self.transmit_message(&pkt)?;
                did_any = true;
            }
            if !drain_fully && self.clock.now_ms().wrapping_sub(start) >= timeout_ms as u64 {
                break;
            }

            // Then RX
            if let Some(item) = {
                let mut st = self.state.lock();
                st.received_queue.pop_front()
            } {
                self.process_rx_queue_item(item)?;
                did_any = true;
            }

            if !drain_fully && self.clock.now_ms().wrapping_sub(start) >= timeout_ms as u64 {
                break;
            }

            if !did_any {
                break;
            }
        }

        Ok(())
    }

    /// Enqueue a telemetry packet for transmission.
    /// This function will validate the packet and add it to the transmit queue.
    /// # Arguments
    /// * `pkt` - The TelemetryPacket to enqueue.
    /// # Returns
    /// A TelemetryResult indicating success or failure.
    pub fn transmit_message_queue(&self, pkt: TelemetryPacket) -> TelemetryResult<()> {
        pkt.validate()?;
        let mut st = self.state.lock();
        st.transmit_queue.push_back(pkt);
        Ok(())
    }

    /// Process all packets in the receive queue.
    /// This function will dequeue and process all packets in the receive queue
    /// until the queue is empty.
    /// # Returns
    /// A TelemetryResult indicating success or failure.
    pub fn process_rx_queue(&self) -> TelemetryResult<()> {
        loop {
            let item_opt = {
                let mut st = self.state.lock();
                st.received_queue.pop_front()
            };
            let Some(item) = item_opt else { break };
            self.process_rx_queue_item(item)?;
        }
        Ok(())
    }

    /// Enqueue a serialized telemetry packet (byte slice) for processing.
    /// This function will add the byte slice to the receive queue.
    /// # Arguments
    /// * `bytes` - The serialized telemetry packet as a byte slice.
    /// # Returns
    /// A TelemetryResult indicating success or failure.
    pub fn rx_serialized_packet_to_queue(&self, bytes: &[u8]) -> TelemetryResult<()> {
        let arc = Arc::from(bytes);
        let mut st = self.state.lock();
        st.received_queue.push_back(RxItem::Serialized(arc));
        Ok(())
    }

    /// Enqueue a telemetry packet for processing.
    /// This function will validate the packet and add it to the receive queue.
    /// # Arguments
    /// * `pkt` - The TelemetryPacket to enqueue.
    /// # Returns
    /// A TelemetryResult indicating success or failure.
    pub fn rx_packet_to_queue(&self, pkt: TelemetryPacket) -> TelemetryResult<()> {
        pkt.validate()?;
        let mut st = self.state.lock();
        st.received_queue.push_back(RxItem::Packet(pkt));
        Ok(())
    }

    /// Retry helper function to attempt a closure multiple times.
    /// # Arguments
    /// * `times` - The number of times to retry.
    /// * `f` - The closure to execute.
    /// # Returns
    /// A Result containing the successful value or the last error.
    fn retry<F, T, E>(&self, times: usize, mut f: F) -> Result<T, E>
    where
        F: FnMut() -> Result<T, E>,
    {
        let mut last_err = None;
        for _ in 0..times {
            match f() {
                Ok(v) => return Ok(v),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.expect("times > 0"))
    }

    /// Check if the specified endpoint has a packet handler registered.
    /// # Arguments
    /// * `ep` - The DataEndpoint to check.
    /// # Returns
    /// `true` if a packet handler is registered for the endpoint, `false` otherwise.
    fn endpoint_has_packet_handler(&self, ep: DataEndpoint) -> bool {
        self.cfg
            .handlers
            .iter()
            .any(|h| h.endpoint == ep && matches!(h.handler, EndpointHandlerFn::Packet(_)))
    }

    /// Check if the specified endpoint has a serialized handler registered.
    /// # Arguments
    /// * `ep` - The DataEndpoint to check.
    /// # Returns
    /// `true` if a serialized handler is registered for the endpoint, `false` otherwise.
    fn endpoint_has_serialized_handler(&self, ep: DataEndpoint) -> bool {
        self.cfg
            .handlers
            .iter()
            .any(|h| h.endpoint == ep && matches!(h.handler, EndpointHandlerFn::Serialized(_)))
    }

    /// Call the specified endpoint handler with retries on failure.
    /// # Arguments
    /// * `dest` - The DataEndpoint for which the handler is registered.
    /// * `handler` - The EndpointHandler to call.
    /// * `data` - The RxItem to pass to the handler (may be None for Packet handlers in send()).
    ///
    /// * `pkt_for_ctx` - The TelemetryPacket context (may be None if not deserialized).
    /// * `env_for_ctx` - The TelemetryEnvelope context (may be None if
    /// not available).
    /// # Returns
    /// A TelemetryResult indicating success or failure.
    fn call_handler_with_retries(
        &self,
        dest: DataEndpoint,
        handler: &EndpointHandler,
        data: Option<&RxItem>, // may be None for Packet handlers in send()
        pkt_for_ctx: Option<&TelemetryPacket>, // may be None if we didn’t deserialize
        env_for_ctx: Option<&serialize::TelemetryEnvelope>, // header-only context
    ) -> TelemetryResult<()> {
        // --- Shared helper function for retry + error handling ---
        fn with_retries<F>(
            this: &Router,
            dest: DataEndpoint,
            pkt_for_ctx: Option<&TelemetryPacket>,
            env_for_ctx: Option<&serialize::TelemetryEnvelope>,
            mut run: F,
        ) -> TelemetryResult<()>
        where
            F: FnMut() -> TelemetryResult<()>,
        {
            this.retry(MAX_HANDLER_RETRIES, || run()).map_err(|e| {
                if let Some(pkt) = pkt_for_ctx {
                    let _ = this.handle_callback_error(pkt, Some(dest), e);
                } else if let Some(env) = env_for_ctx {
                    let _ = this.handle_callback_error_from_env(env, Some(dest), e);
                }
                TelemetryError::HandlerError("local handler failed")
            })
        }

        // --- Dispatch based on handler kind ---
        match (&handler.handler, data) {
            (EndpointHandlerFn::Packet(f), _) => {
                let pkt = pkt_for_ctx.expect("Packet handler requires TelemetryPacket context");
                with_retries(self, dest, pkt_for_ctx, env_for_ctx, || f(pkt))
            }

            (EndpointHandlerFn::Serialized(f), Some(RxItem::Serialized(bytes))) => {
                with_retries(self, dest, pkt_for_ctx, env_for_ctx, || f(bytes))
            }

            (EndpointHandlerFn::Serialized(f), Some(RxItem::Packet(pkt))) => {
                with_retries(self, dest, pkt_for_ctx, env_for_ctx, || {
                    let owned = serialize::serialize_packet(pkt);
                    f(&owned)
                })
            }

            (EndpointHandlerFn::Serialized(_), None) => {
                debug_assert!(
                    false,
                    "Serialized handler called without RxItem; this should not happen"
                );
                Ok(())
            }
        }
    }

    /// Error helper when we only have an envelope (no full packet).
    /// Sends a TelemetryError packet to all local endpoints except the failed one (if any).
    /// If no local endpoints remain, falls back to stdout printing.
    /// # Arguments
    /// * `env` - The TelemetryEnvelope that caused the error.
    /// * `dest` - The endpoint that failed (if any).
    /// * `e` - The TelemetryError that occurred.
    /// # Returns
    /// A TelemetryResult indicating success or failure of sending the error packet.
    /// This is used when we only have the envelope (e.g. for Serialized handlers).
    fn handle_callback_error_from_env(
        &self,
        env: &serialize::TelemetryEnvelope,
        dest: Option<DataEndpoint>,
        e: TelemetryError,
    ) -> TelemetryResult<()> {
        // Decide which local endpoints to target for the error packet:
        // filter to local endpoints; if dest provided, exclude it from the re-broadcast.
        let mut locals: Vec<DataEndpoint> = env
            .endpoints
            .iter()
            .copied()
            .filter(|&ep| self.cfg.is_local_endpoint(ep))
            .collect();
        locals.sort_unstable();
        locals.dedup();
        if let Some(failed) = dest {
            locals.retain(|&ep| ep != failed);
        }

        let error_msg = format!(
            "Handler for endpoint {:?} failed on device {:?}: {:?}",
            dest, DEVICE_IDENTIFIER, e
        );
        if locals.is_empty() {
            fallback_stdout(&error_msg);
            return Ok(());
        }

        let payload = make_error_payload(&error_msg);

        // Build a minimal error packet using envelope fields for sender/timestamp.
        let error_pkt = TelemetryPacket::new(
            DataType::TelemetryError,
            &locals,
            env.sender.clone(),
            env.timestamp_ms,
            payload,
        )?;
        self.transmit_message(&error_pkt)
    }

    /// Core receive function handling both Packet and Serialized RxItems.
    /// Routes to appropriate local handlers with retries and error handling.
    /// # Arguments
    /// * `item` - The RxItem to process (either Packet or Serialized).
    /// # Returns
    /// A TelemetryResult indicating success or failure.
    pub fn receive_item(&self, item: &RxItem) -> TelemetryResult<()> {
        match item {
            RxItem::Packet(pkt) => {
                pkt.validate()?;
                // No pre-serialization; serialize only if a Serialized handler exists.
                for dest in pkt.endpoints().iter().copied() {
                    for h in self.cfg.handlers.iter().filter(|h| h.endpoint == dest) {
                        self.call_handler_with_retries(dest, h, Some(item), Some(pkt), None)?;
                    }
                }
                Ok(())
            }
            RxItem::Serialized(bytes) => {
                // 1) Peek envelope to know endpoints (cheap, no payload copy).
                let env = serialize::peek_envelope(bytes)?;

                // 2) Determine if ANY packet handler exists for the addressed endpoints.
                let any_packet_needed = env
                    .endpoints
                    .iter()
                    .copied()
                    .any(|ep| self.endpoint_has_packet_handler(ep));

                // 3) Deserialize ONCE only if at least one packet-handler is present.
                let pkt_opt = if any_packet_needed {
                    let pkt = serialize::deserialize_packet(bytes)?;
                    pkt.validate()?;
                    Some(pkt)
                } else {
                    None
                };

                // 4) Route to all handlers; for serialized-handlers pass bytes;
                //    for packet-handlers pass the (maybe) deserialized packet.
                for dest in env.endpoints.iter().copied() {
                    for h in self.cfg.handlers.iter().filter(|h| h.endpoint == dest) {
                        match (&h.handler, &pkt_opt) {
                            (EndpointHandlerFn::Packet(_), Some(pkt)) => {
                                self.call_handler_with_retries(
                                    dest,
                                    h,
                                    Some(item),
                                    Some(pkt),
                                    Some(&env),
                                )?;
                            }
                            (EndpointHandlerFn::Packet(_), None) => {
                                // Shouldn’t happen because any_packet_needed would be true.
                                // But guard anyway with a fast path: deserialize just-in-time.
                                let pkt = serialize::deserialize_packet(bytes)?;
                                pkt.validate()?;
                                self.call_handler_with_retries(
                                    dest,
                                    h,
                                    Some(item),
                                    Some(&pkt),
                                    Some(&env),
                                )?;
                            }
                            (EndpointHandlerFn::Serialized(_), _) => {
                                self.call_handler_with_retries(
                                    dest,
                                    h,
                                    Some(item),
                                    pkt_opt.as_ref(),
                                    Some(&env),
                                )?;
                            }
                        }
                    }
                }
                Ok(())
            }
        }
    }

    // send() stays efficient: we already have a packet and must serialize once for
    // remote TX and for any local Serialized handlers. If *no* remote and *no*
    // local Serialized handlers exist, you can skip serialization:

    /// Transmit a telemetry packet immediately.
    /// This function will serialize the packet if needed and send it to remote
    /// endpoints and local handlers.
    /// # Arguments
    /// * `pkt` - The TelemetryPacket to transmit.
    /// # Returns
    /// A TelemetryResult indicating success or failure.
    pub fn transmit_message(&self, pkt: &TelemetryPacket) -> TelemetryResult<()> {
        pkt.validate()?;

        let has_serialized_local = pkt
            .endpoints()
            .iter()
            .copied()
            .any(|ep| self.endpoint_has_serialized_handler(ep));
        let send_remote = pkt
            .endpoints()
            .iter()
            .any(|e| !self.cfg.is_local_endpoint(*e));

        // Only serialize if needed.
        let bytes_opt = if has_serialized_local || send_remote {
            Some(serialize::serialize_packet(pkt))
        } else {
            None
        };

        if send_remote {
            if let (Some(tx), Some(bytes)) = (&self.transmit, &bytes_opt) {
                if let Err(e) = self.retry(MAX_HANDLER_RETRIES, || tx(bytes)) {
                    let _ = self.handle_callback_error(pkt, None, e);
                    return Err(TelemetryError::HandlerError("TX failed"));
                }
            }
        }

        // Local dispatch using the best available representation.
        for dest in pkt.endpoints().iter().copied() {
            for h in self.cfg.handlers.iter().filter(|h| h.endpoint == dest) {
                match (&h.handler, &bytes_opt) {
                    (EndpointHandlerFn::Serialized(_), Some(bytes)) => {
                        let item = RxItem::Serialized(bytes.clone());
                        self.call_handler_with_retries(dest, h, Some(&item), Some(pkt), None)?;
                    }
                    (EndpointHandlerFn::Serialized(_), None) => {
                        // Serialize only for this handler (rare path).
                        let bytes = serialize::serialize_packet(pkt);
                        let item = RxItem::Serialized(Arc::from(bytes));
                        self.call_handler_with_retries(dest, h, Some(&item), Some(pkt), None)?;
                    }
                    (EndpointHandlerFn::Packet(_), _) => {
                        // No need to clone the packet; the handler uses `pkt_for_ctx`.
                        self.call_handler_with_retries(dest, h, None, Some(pkt), None)?;
                    }
                }
            }
        }
        Ok(())
    }

    // receive_serialized / receive can stay as thin wrappers:
    /// Enqueue and process a serialized telemetry packet (byte slice).
    /// This function will create an RxItem from the byte slice and process it.
    /// # Arguments
    /// * `bytes` - The serialized telemetry packet as a byte slice.
    /// # Returns
    /// A TelemetryResult indicating success or failure.
    pub fn receive_serialized(&self, bytes: &[u8]) -> TelemetryResult<()> {
        let item = RxItem::Serialized(Arc::from(bytes));
        self.receive_item(&item)
    }

    /// Enqueue and process a telemetry packet.
    /// This function will create an RxItem from the packet and process it.
    /// # Arguments
    /// * `pkt` - The TelemetryPacket to process.
    /// # Returns
    /// A TelemetryResult indicating success or failure.
    pub fn receive(&self, pkt: &TelemetryPacket) -> TelemetryResult<()> {
        let item = RxItem::Packet(pkt.clone());
        self.receive_item(&item)
    }

    /// Build a packet then send immediately.
    /// # Arguments
    /// * `ty` - The DataType of the packet.
    /// * `data` - The slice of data to log.
    /// # Returns
    /// A TelemetryResult indicating success or failure.
    pub fn log<T: LeBytes>(&self, ty: DataType, data: &[T]) -> TelemetryResult<()> {
        log_raw(self.sender, ty, data, self.clock.now_ms(), |pkt| {
            self.transmit_message(&pkt)
        })
    }

    /// Build a packet and queue it for later TX.
    /// # Arguments
    /// * `ty` - The DataType of the packet.
    /// * `data` - The slice of data to log.
    /// # Returns
    /// A TelemetryResult indicating success or failure.
    pub fn log_queue<T: LeBytes>(&self, ty: DataType, data: &[T]) -> TelemetryResult<()> {
        log_raw(self.sender, ty, data, self.clock.now_ms(), |pkt| {
            self.transmit_message_queue(pkt)
        })
    }

    /// Build a packet with a specific timestamp then send immediately.
    /// # Arguments
    /// * `ty` - The DataType of the packet.
    /// * `timestamp` - The timestamp for the packet.
    /// * `data` - The slice of data to log.
    /// # Returns
    /// A TelemetryResult indicating success or failure.
    pub fn log_ts<T: LeBytes>(
        &self,
        ty: DataType,
        timestamp: u64,
        data: &[T],
    ) -> TelemetryResult<()> {
        log_raw(self.sender, ty, data, timestamp, |pkt| {
            self.transmit_message(&pkt)
        })
    }

    /// Build a packet with a specific timestamp and queue it for later TX.
    /// # Arguments
    /// * `ty` - The DataType of the packet.
    /// * `timestamp` - The timestamp for the packet.
    /// * `data` - The slice of data to log.
    /// # Returns
    /// A TelemetryResult indicating success or failure.
    pub fn log_queue_ts<T: LeBytes>(
        &self,
        ty: DataType,
        timestamp: u64,
        data: &[T],
    ) -> TelemetryResult<()> {
        log_raw(self.sender, ty, data, timestamp, |pkt| {
            self.transmit_message_queue(pkt)
        })
    }
}
