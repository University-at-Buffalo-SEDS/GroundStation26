//! Context-bound recovery budget negotiation used by Mesh and UART admission.
//! All returned frames must use ordinary charged admission; no air-credit API.
//! Existing HEALTH/READY payloads and readiness leases remain unchanged.
const KIND: u8 = 13;
const RF_KIND: u8 = 14;
const LEASE_MS: u64 = 2_000;
const OFFER_HISTORY: usize = 3;
const RETRY_MS: u64 = 1_000;
const MAX_RETRY_MS: u64 = 8_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Nonce([u8; 16]);
impl Nonce {
    pub(crate) fn new(bytes: [u8; 16]) -> Option<Self> {
        bytes.iter().any(|b| *b != 0).then_some(Self(bytes))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Profile {
    pub(crate) baud: u32,
    pub(crate) usable_bps: u32,
    pub(crate) rf_share: u8,
    pub(crate) overhead: u16,
    pub(crate) burst_ms: u16,
    pub(crate) gap_ms: u16,
    pub(crate) flags: u8,
}
impl Profile {
    #[cfg(test)]
    pub(crate) const MATCHED: Self = Self {
        baud: 115_200,
        usable_bps: 32_000,
        rf_share: 35,
        overhead: 16,
        burst_ms: 100,
        gap_ms: 55,
        flags: 0,
    };
    pub(crate) fn valid(self) -> bool {
        self.baud > 0
            && self.baud <= 2_000_000
            && (1200..=1_000_000).contains(&self.usable_bps)
            && (1..=100).contains(&self.rf_share)
            && self.overhead <= 256
            && (1..=500).contains(&self.burst_ms)
            && (1..=1000).contains(&self.gap_ms)
            && self.flags == 0
    }
    pub(crate) fn matches(self, expected: Self) -> bool {
        self == expected && self.valid()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Shares {
    primary_direct: u8,
    primary_fallback: u8,
    relay_base: u8,
    relay_high: u8,
}
impl Shares {
    pub(crate) const CANDIDATE: Self = Self {
        primary_direct: 25,
        primary_fallback: 15,
        relay_base: 35,
        relay_high: 50,
    };
    pub(crate) fn valid(self, profile: Profile) -> bool {
        // Legacy RF40 cannot expand; old healthy25+RF40+base35 remains bounded.
        profile.valid()
            && u16::from(self.primary_direct) + 40 + u16::from(self.relay_base) <= 100
            && u16::from(self.primary_fallback)
                + u16::from(profile.rf_share)
                + u16::from(self.relay_high)
                <= 100
            && u16::from(self.primary_direct)
                + u16::from(profile.rf_share)
                + u16::from(self.relay_base)
                <= 100
            && self.primary_fallback <= self.primary_direct
            && self.relay_base <= self.relay_high
    }
}
pub(crate) fn newer(a: u32, b: u32) -> bool {
    let delta = a.wrapping_sub(b);
    delta != 0 && delta <= i32::MAX as u32
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Health {
    pub(crate) seq: u32,
    pub(crate) direct: bool,
}
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct History {
    entries: [Option<Health>; 4],
    next: usize,
    latest: Option<Health>,
    last_false: Option<u32>,
    phase_start: Option<u32>,
}
impl History {
    pub(crate) fn add(&mut self, health: Health) -> bool {
        if self.latest.is_some_and(|old| !newer(health.seq, old.seq)) {
            return false;
        }
        if self.latest.is_none_or(|old| old.direct != health.direct) {
            self.phase_start = Some(health.seq);
        }
        self.entries[self.next] = Some(health);
        self.next = (self.next + 1) % 4;
        self.latest = Some(health);
        if !health.direct {
            self.last_false = Some(health.seq);
        }
        true
    }
    pub(crate) fn has(&self, seq: u32, direct: bool) -> bool {
        self.entries
            .iter()
            .any(|v| *v == Some(Health { seq, direct }))
    }
    pub(crate) fn belongs(&self, seq: u32, direct: bool) -> bool {
        self.current(direct)
            && self
                .phase_start
                .is_some_and(|start| seq == start || newer(seq, start))
            && self
                .latest
                .is_some_and(|latest| self.has(seq, direct) || newer(latest.seq, seq))
    }
    pub(crate) fn current(&self, direct: bool) -> bool {
        self.latest.is_some_and(|h| h.direct == direct)
    }
    pub(crate) fn after_false(&self, seq: u32) -> bool {
        self.last_false.is_none_or(|old| newer(seq, old))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Context {
    pub(crate) session: u32,
    pub(crate) seq: u32,
    pub(crate) nonce: Nonce,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Message {
    Offer {
        context: Context,
        rf_reply: [u8; 42],
    },
    Grant(Context),
    HighAck(Context),
    Revoke(Context),
    BaseAck {
        revoke: Context,
        relay_nonce: Nonce,
    },
}
impl Message {
    fn context(self) -> Context {
        match self {
            Self::Offer { context, .. } => context,
            Self::Grant(c) | Self::HighAck(c) | Self::Revoke(c) => c,
            Self::BaseAck { revoke, .. } => revoke,
        }
    }
}
#[derive(Clone, Copy)]
pub(crate) struct Frame {
    bytes: [u8; 76],
    len: usize,
}
impl Frame {
    pub(crate) fn wire(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}
pub(crate) fn crc(bytes: &[u8]) -> u32 {
    let mut c = u32::MAX;
    for b in bytes {
        c ^= u32::from(*b);
        for _ in 0..8 {
            c = (c >> 1) ^ (0xedb8_8320u32.wrapping_mul(c & 1));
        }
    }
    !c
}
pub(crate) fn u32le(b: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(b.try_into().ok()?))
}
pub(crate) fn u16le(b: &[u8]) -> Option<u16> {
    Some(u16::from_le_bytes(b.try_into().ok()?))
}
pub(crate) fn rf_query(nonce: Nonce) -> [u8; 26] {
    let mut b = [0; 26];
    b[..6].copy_from_slice(&[b'R', b'R', 1, RF_KIND, 1, 0]);
    b[6..22].copy_from_slice(&nonce.0);
    let checksum = crc(&b[..22]);
    b[22..].copy_from_slice(&checksum.to_le_bytes());
    b
}
pub(crate) fn rf_profile(reply: &[u8; 42], nonce: Nonce) -> Option<Profile> {
    if reply[..6] != [b'R', b'R', 1, RF_KIND, 1, 1]
        || reply[6..22] != nonce.0
        || crc(&reply[..38]) != u32le(&reply[38..])?
    {
        return None;
    }
    let p = Profile {
        baud: u32le(&reply[22..26])?,
        usable_bps: u32le(&reply[26..30])?,
        rf_share: reply[30],
        overhead: u16le(&reply[31..33])?,
        burst_ms: u16le(&reply[33..35])?,
        gap_ms: u16le(&reply[35..37])?,
        flags: reply[37],
    };
    (p.baud != 0
        && p.baud <= 2_000_000
        && (1200..=1_000_000).contains(&p.usable_bps)
        && (1..=100).contains(&p.rf_share)
        && p.overhead <= 256
        && (1..=500).contains(&p.burst_ms)
        && (1..=1000).contains(&p.gap_ms)
        && p.flags & !7 == 0)
        .then_some(p)
}
pub(crate) fn encode(message: Message) -> Frame {
    let c = message.context();
    let mut f = Frame {
        bytes: [0; 76],
        len: 34,
    };
    let tag = match message {
        Message::Offer { .. } => 0,
        Message::Grant(_) => 1,
        Message::HighAck(_) => 2,
        Message::Revoke(_) => 3,
        Message::BaseAck { .. } => 4,
    };
    f.bytes[..6].copy_from_slice(&[b'R', b'R', 1, KIND, 1, tag]);
    f.bytes[6..10].copy_from_slice(&c.session.to_le_bytes());
    f.bytes[10..14].copy_from_slice(&c.seq.to_le_bytes());
    f.bytes[14..30].copy_from_slice(&c.nonce.0);
    match message {
        Message::Offer { rf_reply, .. } => {
            f.bytes[30..72].copy_from_slice(&rf_reply);
            f.len = 76;
        }
        Message::BaseAck { relay_nonce, .. } => {
            f.bytes[30..46].copy_from_slice(&relay_nonce.0);
            f.len = 50;
        }
        _ => {}
    }
    let checksum = crc(&f.bytes[..f.len - 4]);
    f.bytes[f.len - 4..f.len].copy_from_slice(&checksum.to_le_bytes());
    f
}
pub(crate) fn decode(bytes: &[u8]) -> Option<Message> {
    if bytes.len() < 34
        || bytes[..5] != [b'R', b'R', 1, KIND, 1]
        || crc(&bytes[..bytes.len() - 4]) != u32le(&bytes[bytes.len() - 4..])?
    {
        return None;
    }
    let c = Context {
        session: u32le(&bytes[6..10])?,
        seq: u32le(&bytes[10..14])?,
        nonce: Nonce::new(bytes[14..30].try_into().ok()?)?,
    };
    match (bytes[5], bytes.len()) {
        (0, 76) => {
            let reply = bytes[30..72].try_into().ok()?;
            rf_profile(&reply, c.nonce)?;
            Some(Message::Offer {
                context: c,
                rf_reply: reply,
            })
        }
        (1, 34) => Some(Message::Grant(c)),
        (2, 34) => Some(Message::HighAck(c)),
        (3, 34) => Some(Message::Revoke(c)),
        (4, 50) => Some(Message::BaseAck {
            revoke: c,
            relay_nonce: Nonce::new(bytes[30..46].try_into().ok()?)?,
        }),
        _ => None,
    }
}
#[derive(Clone, Copy, Default)]
pub(crate) struct Retry {
    attempts: u8,
    due: u64,
}
impl Retry {
    pub(crate) fn ready(self, now: u64) -> bool {
        now >= self.due
    }
    // Call only for an actual FULL successfully drained physical write. Busy,
    // partial/error/no-op never complete a proof, grant airtime or renew a lease.
    pub(crate) fn committed(&mut self, now: u64) {
        let delay = RETRY_MS
            .saturating_mul(1u64 << self.attempts.min(3))
            .min(MAX_RETRY_MS);
        self.attempts = self.attempts.saturating_add(1);
        self.due = now.saturating_add(delay);
    }
}
pub(crate) struct Primary {
    session: u32,
    shares: Shares,
    expected: Profile,
    history: History,
    clock: Option<u64>,
    share: u8,
    revoke: Option<Context>,
    completed_health: Option<(Health, u64)>,
    grant: Option<Context>,
    grant_done: Option<Context>,
    retry: Retry,
}
impl Primary {
    pub(crate) fn new(session: u32, shares: Shares, expected: Profile) -> Option<Self> {
        shares.valid(expected).then_some(Self {
            session,
            shares,
            expected,
            history: History::default(),
            clock: None,
            share: shares.primary_fallback,
            revoke: None,
            completed_health: None,
            grant: None,
            grant_done: None,
            retry: Retry::default(),
        })
    }
    pub(crate) fn time(&mut self, now: u64) -> bool {
        if self.clock.is_some_and(|old| now < old) {
            self.history = History::default();
            self.share = self.shares.primary_fallback;
            self.revoke = None;
            self.completed_health = None;
            self.grant = None;
            self.grant_done = None;
            self.retry = Retry::default();
            self.clock = Some(now);
            return false;
        }
        self.clock = Some(now);
        true
    }
    pub(crate) fn health_issued(&mut self, health: Health, now: u64) -> bool {
        if !self.time(now) {
            return false;
        }
        let previous_direct = self.history.current(true);
        if !self.history.add(health) {
            return false;
        }
        if !health.direct {
            self.share = self.shares.primary_fallback;
            self.revoke = None;
            self.completed_health = None;
            if previous_direct {
                self.grant_done = None;
            }
        } else {
            self.grant = None;
        }
        true
    }
    fn full_health_written(&mut self, session: u32, health: Health, now: u64) {
        if !self.time(now)
            || session != self.session
            || !health.direct
            || !self.history.belongs(health.seq, true)
            || !self.history.after_false(health.seq)
        {
            return;
        }
        if self
            .completed_health
            .is_none_or(|(old, _)| newer(health.seq, old.seq))
        {
            self.completed_health = Some((health, now));
        }
    }
    fn completed_revoke_reference(&self, owned: Context, now: u64) -> Option<Context> {
        if !self.owns_revoke(owned) {
            return None;
        }
        let (health, at) = self.completed_health?;
        if now < at || now.saturating_sub(at) >= LEASE_MS {
            return None;
        }
        let context = Context {
            seq: health.seq,
            ..owned
        };
        self.owns_revoke(context).then_some(context)
    }
    pub(crate) fn start_revoke(&mut self, challenge: Nonce, now: u64) -> Option<Message> {
        if !self.time(now)
            || !self.history.current(true)
            || self.share == self.shares.primary_direct
        {
            return None;
        }
        if self.revoke.is_some() {
            return self.retry_message(now);
        }
        let seq = self.history.latest?.seq;
        if !self.history.after_false(seq) {
            return None;
        }
        self.share = self.shares.primary_fallback;
        let context = Context {
            session: self.session,
            seq,
            nonce: challenge,
        };
        self.revoke = Some(context);
        self.grant = None;
        self.retry = Retry::default();
        Some(Message::Revoke(context))
    }
    // One challenge owns the uninterrupted issued-true phase, including delayed
    // replies to an earlier retry. Refreshing its wire seq never renews authority.
    fn owns_revoke(&self, context: Context) -> bool {
        self.revoke.is_some_and(|owned| {
            context.session == self.session
                && context.session == owned.session
                && context.nonce == owned.nonce
                && (context.seq == owned.seq || newer(context.seq, owned.seq))
                && self.history.belongs(context.seq, true)
                && self.history.after_false(context.seq)
        })
    }
    pub(crate) fn incoming(&mut self, message: Message, now: u64) -> Option<Message> {
        if !self.time(now) {
            return None;
        }
        match message {
            Message::Offer { context, rf_reply }
                if context.session == self.session
                    && self.history.current(false)
                    && self.history.belongs(context.seq, false)
                    && self.grant_done != Some(context)
                    && rf_profile(&rf_reply, context.nonce)?.matches(self.expected) =>
            {
                self.share = self.shares.primary_fallback;
                self.revoke = None;
                if self.grant != Some(context) {
                    self.grant = Some(context);
                    self.retry = Retry::default();
                }
                self.retry.ready(now).then_some(Message::Grant(context))
            }
            Message::HighAck(context)
                if self.grant == Some(context) && self.history.current(false) =>
            {
                self.share = self.shares.primary_fallback;
                self.grant_done = Some(context);
                self.grant = None;
                None
            }
            Message::BaseAck { revoke, .. } if self.owns_revoke(revoke) => {
                self.share = self.shares.primary_direct;
                self.revoke = None;
                self.grant = None;
                None
            }
            _ => None,
        }
    }
    pub(crate) fn share(&mut self, now: u64) -> u8 {
        self.time(now);
        self.share
    }
    pub(crate) fn retry_message(&mut self, now: u64) -> Option<Message> {
        if !self.time(now) || !self.retry.ready(now) {
            return None;
        }
        self.revoke
            .filter(|_| self.history.current(true))
            .and_then(|owned| {
                self.history.latest.map(|health| {
                    Message::Revoke(Context {
                        seq: health.seq,
                        ..owned
                    })
                })
            })
            .or_else(|| self.grant.map(Message::Grant))
    }
    pub(crate) fn committed(&mut self, message: Message, now: u64, full_drained: bool) {
        if !self.time(now) || !full_drained {
            return;
        }
        if matches!(message,Message::Revoke(c)if self.owns_revoke(c))
            || matches!(message,Message::Grant(c)if self.grant==Some(c))
        {
            self.retry.committed(now);
        }
    }
}
pub(crate) struct Relay {
    shares: Shares,
    expected: Profile,
    nonce: Nonce,
    nonce_live: bool,
    session: Option<u32>,
    history: History,
    last_health: Option<u64>,
    clock: Option<u64>,
    rf_reply: Option<[u8; 42]>,
    high: bool,
    offered: Option<Context>,
    grant: Option<Context>,
    revoke: Option<Context>,
    base_ack: Option<Message>,
    response_pending: bool,
    retry: Retry,
    response_retry: Retry,
    query_retry: Retry,
    offered_seqs: [Option<u32>; 3],
}
impl Relay {
    pub(crate) fn new(nonce: Nonce, shares: Shares, expected: Profile) -> Option<Self> {
        shares.valid(expected).then_some(Self {
            shares,
            expected,
            nonce,
            nonce_live: true,
            session: None,
            history: History::default(),
            last_health: None,
            clock: None,
            rf_reply: None,
            high: false,
            offered: None,
            grant: None,
            revoke: None,
            base_ack: None,
            response_pending: false,
            retry: Retry::default(),
            response_retry: Retry::default(),
            query_retry: Retry::default(),
            offered_seqs: [None; 3],
        })
    }
    // Fresh entropy is injected by the owner (SystemRandom), never inferred from
    // clock/session. Repeated/zero nonce fails closed with no expansion enabled.
    pub(crate) fn rotate(&mut self, next: Nonce) -> bool {
        self.high = false;
        self.rf_reply = None;
        self.offered = None;
        self.grant = None;
        self.revoke = None;
        self.base_ack = None;
        self.response_pending = false;
        self.retry = Retry::default();
        self.response_retry = Retry::default();
        self.query_retry = Retry::default();
        self.offered_seqs = [None; 3];
        self.nonce_live = next != self.nonce;
        if !self.nonce_live {
            return false;
        }
        self.nonce = next;
        true
    }
    pub(crate) fn time(&mut self, now: u64, next: Nonce) -> bool {
        let reset = self.clock.is_some_and(|old| now < old)
            || self
                .last_health
                .is_some_and(|last| now.saturating_sub(last) >= LEASE_MS);
        self.clock = Some(now);
        if reset {
            self.rotate(next);
            self.session = None;
            self.history = History::default();
            self.last_health = None;
            return false;
        }
        true
    }
    pub(crate) fn health(&mut self, session: u32, health: Health, now: u64, next: Nonce) -> bool {
        let reset = !self.time(now, next);
        if self.session != Some(session) {
            if (!reset || !self.nonce_live) && !self.rotate(next) {
                return false;
            }
            self.session = Some(session);
            self.history = History::default();
        }
        let previous_fallback = self.history.current(false);
        let previous_direct = self.history.current(true);
        if !self.history.add(health) {
            return false;
        }
        self.last_health = Some(now);
        if health.direct && previous_fallback && !self.rotate(next) {
            return false;
        }
        if health.direct {
            self.high = false;
            self.offered = None;
            self.grant = None;
        }
        if !health.direct && previous_direct {
            self.revoke = None;
            self.base_ack = None;
            self.response_pending = false;
            self.response_retry = Retry::default();
            self.retry = Retry::default();
            self.query_retry = Retry::default();
        }
        true
    }
    pub(crate) fn profile_observed(&mut self, reply: [u8; 42]) -> Option<Profile> {
        if !self.nonce_live {
            return None;
        }
        let profile = rf_profile(&reply, self.nonce)?;
        self.rf_reply = Some(reply);
        if !profile.matches(self.expected) {
            self.high = false;
            self.grant = None;
            self.offered = None;
        }
        Some(profile)
    }
    pub(crate) fn share(&mut self, now: u64, next: Nonce) -> u8 {
        self.time(now, next);
        if self.high && self.history.current(false) {
            self.shares.relay_high
        } else {
            self.shares.relay_base
        }
    }
    pub(crate) fn offer(&mut self, now: u64, next: Nonce) -> Option<Message> {
        if !self.time(now, next)
            || !self.nonce_live
            || !self.history.current(false)
            || self.high
            || !self.retry.ready(now)
        {
            return None;
        }
        let context = Context {
            session: self.session?,
            seq: self.history.latest?.seq,
            nonce: self.nonce,
        };
        let reply = self.rf_reply?;
        if !rf_profile(&reply, self.nonce)?.matches(self.expected) {
            return None;
        }
        self.offered = Some(context);
        Some(Message::Offer {
            context,
            rf_reply: reply,
        })
    }
    pub(crate) fn incoming(&mut self, message: Message, now: u64, next: Nonce) -> Option<Message> {
        if !self.time(now, next) {
            return None;
        }
        match message {
            Message::Grant(context)
                if self.nonce_live
                    && Some(context.session) == self.session
                    && context.nonce == self.nonce
                    && self.history.current(false)
                    && self.history.belongs(context.seq, false)
                    && (self.offered_seqs.contains(&Some(context.seq))
                        || self.grant == Some(context))
                    && self.rf_reply.is_some_and(|reply| {
                        rf_profile(&reply, self.nonce).is_some_and(|p| p.matches(self.expected))
                    }) =>
            {
                if !self.high {
                    self.response_retry = Retry::default();
                }
                self.high = true;
                self.grant = Some(context);
                self.offered = None;
                self.base_ack = None;
                if self.response_retry.ready(now) {
                    self.response_pending = true;
                    Some(Message::HighAck(context))
                } else {
                    None
                }
            }
            Message::Revoke(context)
                if Some(context.session) == self.session
                    && self.history.current(true)
                    && self.history.belongs(context.seq, true)
                    && self.history.after_false(context.seq) =>
            {
                // Decrease and rotate before constructing the acknowledgement.
                if self.revoke.is_some_and(|owned| {
                    owned.session == context.session && owned.nonce == context.nonce
                }) {
                    if self.response_retry.ready(now) {
                        self.response_pending = true;
                        return self.base_ack;
                    }
                    return None;
                }
                if !self.rotate(next) {
                    return None;
                }
                let ack = Message::BaseAck {
                    revoke: context,
                    relay_nonce: self.nonce,
                };
                self.revoke = Some(context);
                self.base_ack = Some(ack);
                self.response_pending = true;
                Some(ack)
            }
            _ => None,
        }
    }
    pub(crate) fn pending_response(&mut self, now: u64, next: Nonce) -> Option<Message> {
        if !self.time(now, next) || !self.response_pending || !self.response_retry.ready(now) {
            return None;
        }
        self.base_ack
            .or_else(|| self.grant.filter(|_| self.high).map(Message::HighAck))
    }
    pub(crate) fn profile_query(&mut self, now: u64, next: Nonce) -> Option<[u8; 26]> {
        if !self.time(now, next)
            || !self.nonce_live
            || self.history.latest.is_none()
            || self.rf_reply.is_some()
            || !self.query_retry.ready(now)
        {
            return None;
        }
        Some(rf_query(self.nonce))
    }
    pub(crate) fn query_committed(
        &mut self,
        query: &[u8],
        now: u64,
        next: Nonce,
        full_drained: bool,
    ) {
        if self.time(now, next) && full_drained && query == rf_query(self.nonce) {
            self.query_retry.committed(now);
        }
    }
    pub(crate) fn committed(
        &mut self,
        message: Message,
        now: u64,
        next: Nonce,
        full_drained: bool,
    ) {
        if !self.time(now, next) || !full_drained {
            return;
        }
        if let Message::Offer { context, .. } = message
            && self.offered == Some(context)
        {
            self.offered_seqs[usize::from(self.retry.attempts) % OFFER_HISTORY] = Some(context.seq);
            self.retry.committed(now);
        }
        if matches!(message,Message::HighAck(c)if self.grant==Some(c))
            || self.base_ack == Some(message)
        {
            self.response_pending = false;
            self.response_retry.committed(now);
        }
    }
}

/// One logical pending handshake. The physical owner uses Mesh's existing
/// accounted queue; this state contains only fixed metadata, no hidden Vec.
enum State {
    Primary(Primary),
    Relay(Relay),
}
pub(crate) struct Driver {
    state: State,
    next: Option<Nonce>,
    entropy: fn() -> Option<Nonce>,
}
impl Driver {
    pub(crate) fn new(
        primary: bool,
        session: u32,
        expected: Profile,
        entropy: fn() -> Option<Nonce>,
    ) -> Option<Self> {
        let nonce = entropy()?;
        let next = entropy()?;
        if nonce == next {
            return None;
        }
        let state = if primary {
            State::Primary(Primary::new(session, Shares::CANDIDATE, expected)?)
        } else {
            State::Relay(Relay::new(nonce, Shares::CANDIDATE, expected)?)
        };
        Some(Self {
            state,
            next: Some(next),
            entropy,
        })
    }
    fn renew(&mut self) {
        let Some(next) = self.next else {
            return;
        };
        let consumed = match &self.state {
            State::Primary(p) => p.revoke.is_some_and(|c| c.nonce == next),
            State::Relay(r) => r.nonce == next,
        };
        if consumed {
            self.next = (self.entropy)().filter(|n| *n != next);
        }
    }
    pub(crate) fn health(&mut self, session: u32, seq: u32, direct: bool, now: u64) {
        let Some(next) = self.next else {
            return;
        };
        match &mut self.state {
            State::Primary(p) => {
                p.health_issued(Health { seq, direct }, now);
            }
            State::Relay(r) => {
                r.health(session, Health { seq, direct }, now, next);
            }
        }
        self.renew();
    }
    pub(crate) fn expected(&self) -> Profile {
        match &self.state {
            State::Primary(p) => p.expected,
            State::Relay(r) => r.expected,
        }
    }
    pub(crate) fn fail_closed(&mut self) {
        self.next = None;
        match &mut self.state {
            State::Primary(p) => {
                p.share = p.shares.primary_fallback;
                p.revoke = None;
                p.grant = None;
            }
            State::Relay(r) => {
                r.high = false;
                r.grant = None;
                r.rf_reply = None;
            }
        }
    }
    pub(crate) fn peek_share(&self, now: u64) -> u8 {
        if self.next.is_none() {
            return match self.state {
                State::Primary(_) => 15,
                State::Relay(_) => 35,
            };
        }
        match &self.state {
            State::Primary(p) => {
                if p.clock.is_some_and(|old| now < old) {
                    15
                } else {
                    p.share
                }
            }
            State::Relay(r) => {
                if r.high
                    && r.nonce_live
                    && r.history.current(false)
                    && r.clock.is_none_or(|old| now >= old)
                    && r.last_health
                        .is_some_and(|last| now.saturating_sub(last) < LEASE_MS)
                {
                    r.shares.relay_high
                } else {
                    r.shares.relay_base
                }
            }
        }
    }
    pub(crate) fn share(&mut self, now: u64) -> u8 {
        let Some(next) = self.next else {
            return match self.state {
                State::Primary(_) => 15,
                State::Relay(_) => 35,
            };
        };
        let share = match &mut self.state {
            State::Primary(p) => p.share(now),
            State::Relay(r) => r.share(now, next),
        };
        self.renew();
        share
    }
    pub(crate) fn incoming(&mut self, bytes: &[u8], now: u64) {
        let Some(next) = self.next else {
            return;
        };
        if bytes.get(3) == Some(&RF_KIND) {
            if let State::Relay(r) = &mut self.state
                && let Ok(reply) = <&[u8; 42]>::try_from(bytes)
            {
                r.time(now, next);
                r.profile_observed(*reply);
            }
        } else if let Some(message) = decode(bytes) {
            match &mut self.state {
                State::Primary(p) => {
                    p.incoming(message, now);
                }
                State::Relay(r) => {
                    r.incoming(message, now, next);
                }
            }
        }
        self.renew();
    }
    pub(crate) fn prepare(&mut self, now: u64) -> Option<Frame> {
        let next = self.next?;
        let frame = match &mut self.state {
            State::Primary(p) => {
                let message = p.retry_message(now).or_else(|| p.start_revoke(next, now))?;
                encode(message)
            }
            State::Relay(r) => {
                if let Some(message) = r.pending_response(now, next) {
                    encode(message)
                } else if let Some(query) = r.profile_query(now, next) {
                    let mut f = Frame {
                        bytes: [0; 76],
                        len: 26,
                    };
                    f.bytes[..26].copy_from_slice(&query);
                    f
                } else {
                    encode(r.offer(now, next)?)
                }
            }
        };
        self.renew();
        Some(frame)
    }
    pub(crate) fn full_health_written(&mut self, session: u32, seq: u32, direct: bool, now: u64) {
        if let State::Primary(p) = &mut self.state {
            p.full_health_written(session, Health { seq, direct }, now);
        }
    }
    pub(crate) fn refresh_unreserved_revoke(&self, bytes: &[u8], now: u64) -> Option<Frame> {
        let (State::Primary(p), Some(Message::Revoke(context))) = (&self.state, decode(bytes))
        else {
            return None;
        };
        Some(encode(Message::Revoke(
            p.completed_revoke_reference(context, now)?,
        )))
    }
    pub(crate) fn unreserved_revoke_ready(&self, bytes: &[u8], now: u64) -> bool {
        let Some(Message::Revoke(context)) = decode(bytes) else {
            return true;
        };
        matches!(&self.state,State::Primary(p)if p.completed_revoke_reference(context,now)==Some(context))
    }
    pub(crate) fn valid(&mut self, bytes: &[u8], now: u64) -> bool {
        self.share(now);
        if self.next.is_none() {
            return false;
        }
        if bytes.get(3) == Some(&RF_KIND) {
            return matches!(&self.state,State::Relay(r)if r.nonce_live && r.history.latest.is_some()&&r.rf_reply.is_none()&&bytes==rf_query(r.nonce));
        }
        let Some(message) = decode(bytes) else {
            return false;
        };
        match (&self.state, message) {
            (State::Primary(p), Message::Grant(c)) => {
                p.grant == Some(c) && p.history.current(false)
            }
            (State::Primary(p), Message::Revoke(c)) => p.owns_revoke(c),
            (State::Relay(r), Message::Offer { context, .. }) => {
                r.offered == Some(context)
                    && r.history.current(false)
                    && r.nonce_live
                    && context.nonce == r.nonce
            }
            (State::Relay(r), Message::HighAck(c)) => {
                r.high && r.grant == Some(c) && r.history.current(false)
            }
            (State::Relay(r), Message::BaseAck { .. }) => {
                r.base_ack == Some(message) && r.history.current(true)
            }
            _ => false,
        }
    }
    pub(crate) fn committed(&mut self, bytes: &[u8], now: u64, full_drained: bool) {
        let Some(next) = self.next else {
            return;
        };
        match &mut self.state {
            State::Primary(p) => {
                if let Some(message) = decode(bytes) {
                    p.committed(message, now, full_drained);
                }
            }
            State::Relay(r) => {
                if bytes.get(3) == Some(&RF_KIND) {
                    r.query_committed(bytes, now, next, full_drained);
                } else if let Some(message) = decode(bytes) {
                    r.committed(message, now, next, full_drained);
                }
            }
        }
        self.renew();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn n(value: u8) -> Nonce {
        Nonce::new([value; 16]).unwrap()
    }
    fn reply(nonce: Nonce, profile: Profile) -> [u8; 42] {
        let mut b = [0; 42];
        b[..6].copy_from_slice(&[b'R', b'R', 1, 14, 1, 1]);
        b[6..22].copy_from_slice(&nonce.0);
        b[22..26].copy_from_slice(&profile.baud.to_le_bytes());
        b[26..30].copy_from_slice(&profile.usable_bps.to_le_bytes());
        b[30] = profile.rf_share;
        b[31..33].copy_from_slice(&profile.overhead.to_le_bytes());
        b[33..35].copy_from_slice(&profile.burst_ms.to_le_bytes());
        b[35..37].copy_from_slice(&profile.gap_ms.to_le_bytes());
        b[37] = profile.flags;
        let c = crc(&b[..38]);
        b[38..].copy_from_slice(&c.to_le_bytes());
        b
    }
    fn pair() -> (Primary, Relay) {
        let mut p = Primary::new(7, Shares::CANDIDATE, Profile::MATCHED).unwrap();
        let mut r = Relay::new(n(1), Shares::CANDIDATE, Profile::MATCHED).unwrap();
        assert!(p.health_issued(
            Health {
                seq: 1,
                direct: false
            },
            0
        ));
        assert!(r.health(
            7,
            Health {
                seq: 1,
                direct: false
            },
            0,
            n(2)
        ));
        assert!(
            r.profile_observed(reply(r.nonce, Profile::MATCHED))
                .unwrap()
                .matches(Profile::MATCHED)
        );
        (p, r)
    }
    fn high() -> (Primary, Relay, Message) {
        let (mut p, mut r) = pair();
        let offer = r.offer(1, n(9)).unwrap();
        r.committed(offer, 1, n(9), true);
        let grant = p.incoming(offer, 2).unwrap();
        p.committed(grant, 2, true);
        let ack = r.incoming(grant, 3, n(9)).unwrap();
        r.committed(ack, 3, n(9), true);
        p.incoming(ack, 4);
        assert_eq!(p.share, 15);
        assert_eq!(r.share(4, n(9)), 50);
        (p, r, grant)
    }
    #[test]
    fn defaults_and_conservation_reject_bad_profiles() {
        assert!(Shares::CANDIDATE.valid(Profile::MATCHED));
        let mut bad = Profile::MATCHED;
        bad.rf_share = 40;
        assert!(!Shares::CANDIDATE.valid(bad));
        let bad = Shares {
            relay_high: 51,
            ..Shares::CANDIDATE
        };
        assert!(Primary::new(7, bad, Profile::MATCHED).is_none());
        let p = Primary::new(7, Shares::CANDIDATE, Profile::MATCHED).unwrap();
        let mut r = Relay::new(n(1), Shares::CANDIDATE, Profile::MATCHED).unwrap();
        assert_eq!(p.share, 15);
        assert_eq!(r.share(0, n(9)), 35);
    }
    #[test]
    fn rf_exact_wire_sizes_crc_nonce_and_mismatches() {
        let query = rf_query(n(2));
        assert_eq!(query.len(), 26);
        assert_eq!(
            query,
            [
                82, 82, 1, 14, 1, 0, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 50, 21, 14, 7
            ]
        );
        assert_eq!(&query[..6], b"RR\x01\x0e\x01\x00");
        assert_eq!(crc(&query[..22]), u32le(&query[22..]).unwrap());
        let good = reply(n(2), Profile::MATCHED);
        assert_eq!(
            good,
            [
                82, 82, 1, 14, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 0, 194, 1, 0,
                0, 125, 0, 0, 35, 16, 0, 100, 0, 55, 0, 0, 12, 214, 14, 180
            ]
        );
        assert_eq!(rf_profile(&good, n(2)), Some(Profile::MATCHED));
        assert!(rf_profile(&good, n(3)).is_none());
        let mut bad = good;
        bad[41] ^= 1;
        assert!(rf_profile(&bad, n(2)).is_none());
        for flags in [1, 2, 4] {
            let observed = Profile {
                flags,
                ..Profile::MATCHED
            };
            assert!(
                !rf_profile(&reply(n(2), observed), n(2))
                    .unwrap()
                    .matches(Profile::MATCHED)
            );
        }
        let observed = Profile {
            rf_share: 40,
            ..Profile::MATCHED
        };
        assert_eq!(rf_profile(&reply(n(2), observed), n(2)), Some(observed));
        assert!(!observed.matches(Profile::MATCHED));
        assert!(Nonce::new([0; 16]).is_none());
    }
    #[test]
    fn wire_messages_exact_roundtrip_reject_trailing_tags_and_zero() {
        let c = Context {
            session: 7,
            seq: 1,
            nonce: n(2),
        };
        for message in [
            Message::Offer {
                context: c,
                rf_reply: reply(n(2), Profile::MATCHED),
            },
            Message::Grant(c),
            Message::HighAck(c),
            Message::Revoke(c),
            Message::BaseAck {
                revoke: c,
                relay_nonce: n(3),
            },
        ] {
            let frame = encode(message);
            assert_eq!(decode(frame.wire()), Some(message));
            let mut bad = frame;
            bad.bytes[5] = 99;
            let v = crc(&bad.bytes[..bad.len - 4]);
            bad.bytes[bad.len - 4..bad.len].copy_from_slice(&v.to_le_bytes());
            assert!(decode(bad.wire()).is_none());
            let mut extra = frame.wire().to_vec();
            extra.push(0);
            assert!(decode(&extra).is_none());
            let mut zero = frame;
            zero.bytes[14..30].fill(0);
            let v = crc(&zero.bytes[..zero.len - 4]);
            zero.bytes[zero.len - 4..zero.len].copy_from_slice(&v.to_le_bytes());
            assert!(decode(zero.wire()).is_none());
        }
    }
    #[test]
    fn partial_offer_cannot_authorize_and_duplicate_grant_is_bounded() {
        let (mut p, mut r) = pair();
        let offer = r.offer(1, n(9)).unwrap();
        r.committed(offer, 1, n(9), false);
        let grant = p.incoming(offer, 2).unwrap();
        assert!(r.incoming(grant, 3, n(9)).is_none());
        assert_eq!(r.share(3, n(9)), 35);
        r.committed(offer, 4, n(9), true);
        let ack = r.incoming(grant, 5, n(9)).unwrap();
        r.committed(ack, 5, n(9), true);
        assert!(r.incoming(grant, 6, n(9)).is_none());
        assert!(r.health(
            7,
            Health {
                seq: 2,
                direct: false
            },
            500,
            n(9)
        ));
        let ack = r.incoming(grant, 1005, n(9)).unwrap();
        r.committed(ack, 1005, n(9), true);
        assert_eq!(r.share(1005, n(9)), 50);
    }
    #[test]
    fn restore_requires_fresh_revoke_ack_and_existing_true_health() {
        let (mut p, mut r, old_grant) = high();
        assert!(p.health_issued(
            Health {
                seq: 2,
                direct: true
            },
            500
        ));
        let revoke = p.start_revoke(n(50), 501).unwrap();
        assert_eq!(p.share, 15);
        assert!(
            r.incoming(revoke, 502, n(3)).is_none(),
            "REVOKE cannot stand in for HEALTH"
        );
        assert_eq!(r.share(502, n(3)), 50);
        assert!(r.health(
            7,
            Health {
                seq: 2,
                direct: true
            },
            503,
            n(3)
        ));
        assert_eq!(r.share(503, n(9)), 35);
        let ack = r.incoming(revoke, 504, n(4)).unwrap();
        assert_eq!(r.share(504, n(9)), 35);
        p.incoming(ack, 505);
        assert_eq!(p.share, 25);
        assert!(r.incoming(old_grant, 506, n(9)).is_none());
        assert_eq!(
            r.incoming(revoke, 506, n(9)),
            Some(ack),
            "same revoke is idempotent before full reply commit"
        );
        r.committed(ack, 506, n(9), true);
        assert!(r.incoming(revoke, 507, n(9)).is_none());
    }
    #[test]
    fn primary_u32_aba_old_base_ack_cannot_match_fresh_challenge() {
        let (mut p, mut r, _) = high();
        p.health_issued(
            Health {
                seq: 2,
                direct: true,
            },
            500,
        );
        r.health(
            7,
            Health {
                seq: 2,
                direct: true,
            },
            500,
            n(3),
        );
        let revoke = p.start_revoke(n(50), 501).unwrap();
        let old_ack = r.incoming(revoke, 502, n(4)).unwrap();
        let mut reboot = Primary::new(7, Shares::CANDIDATE, Profile::MATCHED).unwrap();
        reboot.health_issued(
            Health {
                seq: 2,
                direct: true,
            },
            500,
        );
        reboot.start_revoke(n(51), 501);
        reboot.incoming(old_ack, 503);
        assert_eq!(reboot.share, 15);
    }
    #[test]
    fn relay_restart_and_session_aba_invalidate_old_grants() {
        let (_, mut r, grant) = high();
        let mut reboot = Relay::new(n(80), Shares::CANDIDATE, Profile::MATCHED).unwrap();
        reboot.health(
            7,
            Health {
                seq: 1,
                direct: false,
            },
            0,
            n(81),
        );
        assert!(reboot.incoming(grant, 1, n(9)).is_none());
        assert_eq!(reboot.share(1, n(9)), 35);
        assert!(r.health(
            8,
            Health {
                seq: 0,
                direct: false
            },
            500,
            n(3)
        ));
        assert!(r.health(
            7,
            Health {
                seq: 2,
                direct: false
            },
            501,
            n(4)
        ));
        assert!(r.incoming(grant, 502, n(9)).is_none());
        assert_eq!(r.share(502, n(9)), 35);
    }
    #[test]
    fn lease_expiry_backstep_and_reused_entropy_fail_closed() {
        let (_, mut r, grant) = high();
        assert_eq!(r.share(2000, n(3)), 35);
        assert!(r.incoming(grant, 2001, n(9)).is_none());
        assert!(r.health(
            7,
            Health {
                seq: 2,
                direct: false
            },
            2002,
            n(4)
        ));
        assert_eq!(r.share(10, n(5)), 35);
        assert!(r.incoming(grant, 11, n(9)).is_none());
        let (_, mut r, _) = high();
        let same = r.nonce;
        assert_eq!(r.share(2000, same), 35);
        assert!(r.profile_observed(reply(same, Profile::MATCHED)).is_none());
        assert!(r.offer(2001, n(9)).is_none());
    }
    #[test]
    fn same_context_retry_offer_accepts_earlier_still_recent_health_proof() {
        let (mut p, mut r) = pair();
        let first = r.offer(1, n(9)).unwrap();
        r.committed(first, 1, n(9), true);
        p.health_issued(
            Health {
                seq: 2,
                direct: false,
            },
            500,
        );
        r.health(
            7,
            Health {
                seq: 2,
                direct: false,
            },
            500,
            n(9),
        );
        let second = r.offer(1001, n(9)).unwrap();
        r.committed(second, 1001, n(9), true);
        let old_grant = p.incoming(first, 1002).unwrap();
        assert!(r.incoming(old_grant, 1003, n(9)).is_some());
        assert_eq!(r.share(1003, n(9)), 50);
    }
    #[test]
    fn history_wrap_false_boundary_and_duplicate_health_do_not_renew_lease() {
        let mut h = History::default();
        assert!(h.add(Health {
            seq: u32::MAX - 1,
            direct: false
        }));
        assert!(h.add(Health {
            seq: u32::MAX,
            direct: true
        }));
        assert!(h.add(Health {
            seq: 0,
            direct: true
        }));
        assert!(h.after_false(0));
        assert!(!h.add(Health {
            seq: 0,
            direct: true
        }));
        assert!(h.add(Health {
            seq: 1,
            direct: false
        }));
        assert!(!h.after_false(0));
        let (_, mut r, _) = high();
        assert!(!r.health(
            7,
            Health {
                seq: 1,
                direct: false
            },
            1999,
            n(3)
        ));
        assert_eq!(r.share(2000, n(4)), 35);
    }
    #[test]
    fn bounded_retries_no_busy_spend_no_idle_credit_and_legacy_conservative() {
        let (mut p, mut r) = pair();
        let offer = r.offer(1, n(9)).unwrap();
        r.committed(offer, 1, n(9), false);
        assert!(r.offer(1, n(9)).is_some());
        let grant = p.incoming(offer, 2).unwrap();
        p.committed(grant, 2, false);
        assert!(p.retry_message(2).is_some());
        for step in 0..3u64 {
            let now = 10 + step * 500;
            let seq = 2 + step as u32;
            p.health_issued(Health { seq, direct: false }, now);
            r.health(7, Health { seq, direct: false }, now, n(9));
        }
        // A legacy peer never sends GRANT: observed RF40 also cannot certify one.
        let old = Profile {
            rf_share: 40,
            ..Profile::MATCHED
        };
        r.profile_observed(reply(r.nonce, old));
        assert!(r.offer(1600, n(9)).is_none());
        assert_eq!(r.share(1600, n(9)), 35);
        let mut retry = Retry::default();
        for now in [0, 1000, 3000, 7000, 15000] {
            assert!(retry.ready(now));
            retry.committed(now);
        }
        assert!(retry.ready(99_000));
    }
    #[test]
    fn delayed_grant_is_revoked_even_before_it_was_applied() {
        let (mut p, mut r) = pair();
        let offer = r.offer(1, n(9)).unwrap();
        r.committed(offer, 1, n(9), true);
        let old = p.incoming(offer, 2).unwrap();
        let nonce = r.nonce;
        assert!(r.health(
            7,
            Health {
                seq: 2,
                direct: true
            },
            500,
            n(3)
        ));
        assert_ne!(r.nonce, nonce);
        assert!(r.health(
            7,
            Health {
                seq: 3,
                direct: false
            },
            501,
            n(4)
        ));
        assert!(r.incoming(old, 502, n(9)).is_none());
        assert_eq!(r.share(502, n(9)), 35);
    }
    #[test]
    fn completed_grant_dedup_survives_next_fallback_health() {
        let (mut p, mut r) = pair();
        let offer = r.offer(1, n(9)).unwrap();
        r.committed(offer, 1, n(9), true);
        let grant = p.incoming(offer, 2).unwrap();
        let ack = r.incoming(grant, 3, n(9)).unwrap();
        p.incoming(ack, 4);
        assert!(p.health_issued(
            Health {
                seq: 2,
                direct: false
            },
            500
        ));
        assert!(
            p.incoming(offer, 501).is_none(),
            "500ms HEALTH must not manufacture a new negotiation retry budget"
        );
    }

    #[test]
    fn retry_progresses_after_four_lost_offers_and_stops_after_proof() {
        let (mut p, mut r) = pair();
        let mut last = 0;
        for (attempt, now) in [1, 1001, 3001, 7001, 15001].into_iter().enumerate() {
            while last + 500 <= now {
                last += 500;
                let h = Health {
                    seq: 1 + (last / 500) as u32,
                    direct: false,
                };
                assert!(p.health_issued(h, last));
                assert!(r.health(7, h, last, n(9)));
            }
            let offer = r
                .offer(now, n(9))
                .expect("one retained retry owner recovers after3 losses");
            r.committed(offer, now, n(9), true);
            if attempt == 4 {
                let grant = p.incoming(offer, now + 1).unwrap();
                let ack = r.incoming(grant, now + 2, n(9)).unwrap();
                p.incoming(ack, now + 3);
                assert_eq!(r.share(now + 3, n(9)), 50);
                assert!(r.offer(now + 4, n(9)).is_none());
            }
        }
    }
    #[test]
    fn owned_responses_survive_three_and_five_second_radio_delay() {
        for delay in [3000, 5000] {
            let (mut p, mut r) = pair();
            let offer = r.offer(1, n(9)).unwrap();
            r.committed(offer, 1, n(9), true);
            let grant = p.incoming(offer, 2).unwrap();
            for now in (500..=delay).step_by(500) {
                let h = Health {
                    seq: 1 + now as u32 / 500,
                    direct: false,
                };
                p.health_issued(h, now);
                r.health(7, h, now, n(9));
            }
            assert!(
                r.incoming(grant, delay + 1, n(9)).is_some(),
                "ownedGRANT belongs same uninterruptedfallback, not recent4window"
            );
            let true_seq = 2 + delay as u32 / 500;
            p.health_issued(
                Health {
                    seq: true_seq,
                    direct: true,
                },
                delay + 2,
            );
            r.health(
                7,
                Health {
                    seq: true_seq,
                    direct: true,
                },
                delay + 2,
                n(3),
            );
            let revoke = p.start_revoke(n(50), delay + 3).unwrap();
            let base = r.incoming(revoke, delay + 4, n(4)).unwrap();
            for offset in (500..=delay).step_by(500) {
                let h = Health {
                    seq: true_seq + offset as u32 / 500,
                    direct: true,
                };
                p.health_issued(h, delay + offset + 4);
                r.health(7, h, delay + offset + 4, n(9));
            }
            p.incoming(base, delay * 2 + 5);
            assert_eq!(
                p.share(delay * 2 + 5),
                25,
                "outstandingfreshchallenge retains ownedtruecontext beyond4entrywindow"
            );
        }
    }
    #[test]
    fn query_backoff_mismatch_observation_and_response_retry_are_bounded() {
        let mut r = Relay::new(n(1), Shares::CANDIDATE, Profile::MATCHED).unwrap();
        r.health(
            7,
            Health {
                seq: 1,
                direct: false,
            },
            0,
            n(2),
        );
        let q = r.profile_query(1, n(9)).unwrap();
        r.query_committed(&q, 1, n(9), false);
        assert!(r.profile_query(2, n(9)).is_some());
        r.query_committed(&q, 2, n(9), true);
        assert!(r.profile_query(3, n(9)).is_none());
        let old = Profile {
            rf_share: 40,
            ..Profile::MATCHED
        };
        r.profile_observed(reply(r.nonce, old));
        assert!(
            r.profile_query(1003, n(9)).is_none(),
            "observedlegacy40 isnot repeatedlyqueried"
        );
        assert!(r.offer(1003, n(9)).is_none());
        let (_, mut r, grant) = high();
        assert!(r.pending_response(4, n(9)).is_none());
        assert!(
            r.pending_response(1004, n(9)).is_none(),
            "successfulACKdoesnotcreateperiodictraffic"
        );
        let ack = r.incoming(grant, 1004, n(9)).unwrap();
        r.committed(ack, 1004, n(9), true);
        assert!(r.pending_response(1005, n(9)).is_none());
    }
    #[test]
    fn lost_first_issued_health_refreshes_revoke_without_invalidating_delayed_ack() {
        let mut p = Primary::new(7, Shares::CANDIDATE, Profile::MATCHED).unwrap();
        let mut r = Relay::new(n(1), Shares::CANDIDATE, Profile::MATCHED).unwrap();
        assert!(p.health_issued(
            Health {
                seq: 0,
                direct: true
            },
            0
        ));
        let lost_context = p.start_revoke(n(50), 1).unwrap();
        p.committed(lost_context, 1, true);
        for seq in 1..=10 {
            let now = u64::from(seq) * 500;
            let h = Health { seq, direct: true };
            assert!(p.health_issued(h, now));
            assert!(r.health(7, h, now, n(9)));
        }
        assert!(
            r.incoming(lost_context, 5001, n(4)).is_none(),
            "never accept unobserved true seq0"
        );
        let retry = p.retry_message(5001).unwrap();
        let Message::Revoke(c) = retry else { panic!() };
        assert_eq!(c.seq, 10);
        assert_eq!(c.nonce, n(50));
        p.committed(retry, 5001, true);
        let ack = r
            .incoming(retry, 5002, n(4))
            .expect("retry references received true HEALTH");
        r.committed(ack, 5002, n(9), true);
        let stable_nonce = r.nonce;
        for seq in 11..=20 {
            let now = u64::from(seq) * 500;
            let h = Health { seq, direct: true };
            assert!(p.health_issued(h, now));
            assert!(r.health(7, h, now, n(9)));
        }
        let newer_retry = p.retry_message(10001).unwrap();
        let Message::Revoke(newer) = newer_retry else {
            panic!()
        };
        assert_eq!(newer.seq, 20);
        assert_eq!(newer.nonce, c.nonce);
        p.committed(newer_retry, 10001, true);
        assert_eq!(r.incoming(newer_retry, 10002, n(8)), Some(ack));
        assert_eq!(
            r.nonce, stable_nonce,
            "same challenge retry must not rotate relay nonce"
        );
        let mut restarted = Primary::new(7, Shares::CANDIDATE, Profile::MATCHED).unwrap();
        restarted.health_issued(
            Health {
                seq: 10,
                direct: true,
            },
            10003,
        );
        restarted.start_revoke(n(51), 10003).unwrap();
        restarted.incoming(ack, 10003);
        assert_eq!(
            restarted.share(10003),
            15,
            "same session/sequence ABA cannot match fresh restart challenge"
        );
        p.incoming(ack, 10003);
        assert_eq!(
            p.share(10003),
            25,
            "older owned BASE_ACK survives later HEALTH and refreshed retry"
        );
        assert!(p.health_issued(
            Health {
                seq: 21,
                direct: false
            },
            10500
        ));
        p.incoming(ack, 10501);
        assert_eq!(p.share(10501), 15, "phase flip invalidates challenge proof");
    }
    #[test]
    fn completed_health_marker_requires_physical_phase_session_and_lease_freshness() {
        let mut p = Primary::new(7, Shares::CANDIDATE, Profile::MATCHED).unwrap();
        let h0 = Health {
            seq: 0,
            direct: true,
        };
        p.health_issued(h0, 0);
        let Message::Revoke(c0) = p.start_revoke(n(50), 0).unwrap() else {
            panic!()
        };
        assert!(p.completed_revoke_reference(c0, 0).is_none());
        p.full_health_written(8, h0, 0);
        assert!(p.completed_health.is_none());
        p.full_health_written(7, h0, 0);
        assert_eq!(p.completed_revoke_reference(c0, 1999), Some(c0));
        assert!(p.completed_revoke_reference(c0, 2000).is_none());
        p.health_issued(
            Health {
                seq: 1,
                direct: true,
            },
            500,
        );
        assert_eq!(
            p.completed_revoke_reference(c0, 500),
            Some(c0),
            "issued future HEALTH must not advance the wire marker"
        );
        p.full_health_written(
            7,
            Health {
                seq: 1,
                direct: true,
            },
            510,
        );
        let c1 = Context { seq: 1, ..c0 };
        assert_eq!(p.completed_revoke_reference(c0, 511), Some(c1));
        p.health_issued(
            Health {
                seq: 2,
                direct: false,
            },
            600,
        );
        assert!(p.completed_health.is_none());
        assert!(p.completed_revoke_reference(c0, 600).is_none());
        p.full_health_written(7, h0, 601);
        assert!(p.completed_health.is_none());
        p.health_issued(
            Health {
                seq: 3,
                direct: true,
            },
            700,
        );
        let Message::Revoke(c3) = p.start_revoke(n(51), 700).unwrap() else {
            panic!()
        };
        p.full_health_written(
            7,
            Health {
                seq: 3,
                direct: true,
            },
            710,
        );
        assert!(p.completed_revoke_reference(c0, 710).is_none());
        assert_eq!(p.completed_revoke_reference(c3, 710), Some(c3));
        p.time(709);
        assert!(p.completed_health.is_none());
        assert!(p.revoke.is_none());
        let mut restarted = Primary::new(7, Shares::CANDIDATE, Profile::MATCHED).unwrap();
        restarted.health_issued(
            Health {
                seq: 3,
                direct: true,
            },
            711,
        );
        restarted.start_revoke(n(52), 711);
        assert!(restarted.completed_revoke_reference(c3, 712).is_none());
    }
    #[test]
    fn root_actual_revoke_survives_five_second_paid_delay() {
        let (mut p, mut r, _) = high();
        let first = Health {
            seq: 2,
            direct: true,
        };
        assert!(p.health_issued(first, 10));
        assert!(r.health(7, first, 10, n(3)));
        let revoke = p.start_revoke(n(50), 11).unwrap();
        for seq in 3..=12 {
            let now = 10 + u64::from(seq - 2) * 500;
            let h = Health { seq, direct: true };
            assert!(p.health_issued(h, now));
            assert!(r.health(7, h, now, n(9)));
        }
        let ack = r
            .incoming(revoke, 5011, n(4))
            .expect("already accepted uninterrupted true context must retain delayed REVOKE");
        p.incoming(ack, 5012);
        assert_eq!(p.share(5012), 25);
    }

    #[test]
    fn driver_retains_one_context_and_stops_after_each_complete_proof() {
        fn entropy() -> Option<Nonce> {
            static NEXT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(20);
            Nonce::new([NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst); 16])
        }
        let mut p = Driver::new(true, 7, Profile::MATCHED, entropy).unwrap();
        let mut r = Driver::new(false, 9, Profile::MATCHED, entropy).unwrap();
        assert_eq!(p.expected(), Profile::MATCHED);
        p.health(7, 0, true, 0);
        r.health(7, 0, true, 0);
        let revoke = p.prepare(1).unwrap();
        assert!(p.valid(revoke.wire(), 1));
        p.committed(revoke.wire(), 1, true);
        r.incoming(revoke.wire(), 2);
        let base = r.prepare(2).unwrap();
        assert!(r.valid(base.wire(), 2));
        r.committed(base.wire(), 2, true);
        p.incoming(base.wire(), 3);
        assert_eq!(p.peek_share(3), 25);
        let query = r.prepare(4).unwrap();
        assert_eq!(query.wire()[3], 14);
        assert!(r.valid(query.wire(), 4));
        r.committed(query.wire(), 4, true);
        let nonce = Nonce::new(query.wire()[6..22].try_into().unwrap()).unwrap();
        r.incoming(&reply(nonce, Profile::MATCHED), 5);
        assert!(
            r.prepare(6).is_none(),
            "healthyprofileproofdoesnotforwarddataorcreateperiodicnegotiation"
        );
        p.health(7, 1, false, 500);
        r.health(7, 1, false, 500);
        let offer = r.prepare(501).unwrap();
        r.committed(offer.wire(), 501, true);
        p.incoming(offer.wire(), 502);
        assert_eq!(p.peek_share(502), 15);
        let grant = p.prepare(503).unwrap();
        p.committed(grant.wire(), 503, true);
        r.incoming(grant.wire(), 504);
        assert_eq!(r.peek_share(504), 50);
        let high = r.prepare(505).unwrap();
        r.committed(high.wire(), 505, true);
        p.incoming(high.wire(), 506);
        assert!(p.prepare(507).is_none());
        assert!(r.prepare(507).is_none());
        r.fail_closed();
        assert_eq!(r.peek_share(508), 35);
        assert!(r.prepare(508).is_none());
        assert!(!r.valid(high.wire(), 508));
    }
    #[test]
    fn expected_profile_is_parameterized_not_the_prototype_constant() {
        let expected = Profile {
            overhead: 24,
            burst_ms: 80,
            gap_ms: 65,
            ..Profile::MATCHED
        };
        let mut r = Relay::new(n(1), Shares::CANDIDATE, expected).unwrap();
        r.health(
            7,
            Health {
                seq: 1,
                direct: false,
            },
            0,
            n(2),
        );
        r.profile_observed(reply(r.nonce, Profile::MATCHED));
        assert!(r.offer(1, n(9)).is_none());
        r.profile_observed(reply(r.nonce, expected));
        assert!(r.offer(2, n(9)).is_some());
    }
}
