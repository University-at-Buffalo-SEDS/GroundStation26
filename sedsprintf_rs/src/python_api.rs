//! Python bindings for the SEDS telemetry router.
//!
//! Exposes a small, opinionated API to Python via `pyo3`:
//!
//! - `Packet`
//!   - Immutable view of a `TelemetryPacket`
//!   - Header fields + payload access
//!   - Serialization to bytes
//!
//! - `Router`
//!   - Logging (bytes, f32, generic typed)
//!   - RX/TX queue processing (with optional timeouts)
//!   - Python callbacks for TX, packet handlers, and serialized handlers
//!
//! - Top-level helpers
//!   - `deserialize_packet_py(data)` → `Packet`
//!   - `peek_header_py(data)` → dict with header fields
//!   - `make_packet(...)` → `Packet`
//!
//! - Dynamic enums
//!   - `DataType`, `DataEndpoint`, `ElemKind`
//!
//! The Python module name is `sedsprintf_rs`.

use alloc::{boxed::Box, string::String, sync::Arc as AArc, vec::Vec};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyModule, PyTuple};
use std::sync::{Arc as SArc, Mutex, OnceLock};

use crate::{
    config::DataEndpoint, get_needed_message_size, message_meta, router::{BoardConfig, Clock, EndpointHandler, LeBytes, Router},
    serialize::{deserialize_packet, packet_wire_size, peek_envelope, serialize_packet},
    telemetry_packet::{DataType, TelemetryPacket},
    try_enum_from_u32, MessageElementCount,
    TelemetryError,
    TelemetryResult,
    MAX_VALUE_DATA_ENDPOINT,
    MAX_VALUE_DATA_TYPE,
};

static GLOBAL_ROUTER_SINGLETON: OnceLock<SArc<Mutex<Router>>> = OnceLock::new();

// ============================================================================
//  Shared helpers / constants
// ============================================================================

/// Element-kind tags for typed logging (mirrors C FFI).
const EK_UNSIGNED: u32 = 0;
const EK_SIGNED: u32 = 1;
const EK_FLOAT: u32 = 2;

/// Map `TelemetryError` → `PyErr` with a consistent prefix.
fn py_err_from(e: TelemetryError) -> PyErr {
    PyRuntimeError::new_err(format!("Telemetry error: {e:?}"))
}

/// Convert Python-side `int` → `DataType`, with range/validity checks.
fn dtype_from_u32(x: u32) -> TelemetryResult<DataType> {
    DataType::try_from_u32(x).ok_or(TelemetryError::InvalidType)
}

/// Convert Python-side `int` → `DataEndpoint`, with range/validity checks.
fn endpoint_from_u32(x: u32) -> TelemetryResult<DataEndpoint> {
    DataEndpoint::try_from_u32(x).ok_or(TelemetryError::Deserialize("bad endpoint"))
}

/// Return the fixed payload size in bytes for a type, or `None` if dynamic.
fn required_payload_size_for(ty: DataType) -> Option<usize> {
    match message_meta(ty).element_count {
        MessageElementCount::Static(_) => Some(get_needed_message_size(ty)),
        MessageElementCount::Dynamic => None,
    }
}

/// Reinterpret a byte buffer as a Vec<T> using unaligned little-endian reads,
/// writing into `out` without realloc churn. Panic-safe w.r.t. `set_len`.
fn vectorize_data<T: LeBytes + Copy>(
    base: *const u8,
    count: usize,
    elem_size: usize,
    out: &mut Vec<T>,
) -> Result<(), ()> {
    use core::{mem::size_of, ptr};

    if elem_size != size_of::<T>() || base.is_null() || count == 0 {
        return Err(());
    }

    out.reserve_exact(count);
    unsafe {
        let start_len = out.len();
        let dst = out.as_mut_ptr().add(start_len);
        for i in 0..count {
            let v = ptr::read_unaligned(base.add(i * elem_size) as *const T);
            dst.add(i).write(v);
        }
        out.set_len(start_len + count);
    }
    Ok(())
}

// ============================================================================
//  Packet (PyPacket)
// ============================================================================

/// Python-visible wrapper around `TelemetryPacket`.
///
/// Constructed indirectly via:
/// - `deserialize_packet_py`
/// - `make_packet`
/// - callbacks (router handlers)
#[pyclass(name = "Packet")]
pub struct PyPacket {
    pub(crate) inner: TelemetryPacket,
}

#[pymethods]
impl PyPacket {
    /// DataType as an integer (see `DataType` IntEnum).
    #[getter]
    fn ty(&self) -> u32 {
        self.inner.data_type() as u32
    }

    /// Declared data size for the packet payload, in bytes.
    #[getter]
    fn data_size(&self) -> usize {
        self.inner.data_size()
    }

    /// Sender identifier as a UTF-8 string.
    #[getter]
    fn sender(&self) -> String {
        self.inner.sender().to_string()
    }

    /// Endpoints as integer IDs (see `DataEndpoint` IntEnum).
    #[getter]
    fn endpoints(&self) -> Vec<u32> {
        self.inner.endpoints().iter().map(|e| *e as u32).collect()
    }

    /// Packet timestamp in milliseconds (source-defined semantics).
    #[getter]
    fn timestamp_ms(&self) -> u64 {
        self.inner.timestamp()
    }

    /// Raw payload bytes.
    #[getter]
    fn payload<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner.payload())
    }

    /// Human-readable header string (no payload).
    fn header_string(&self) -> String {
        self.inner.header_string()
    }

    /// Full human-readable representation (header + payload summary).
    fn __str__(&self) -> String {
        self.inner.to_string()
    }

    /// Wire size in bytes when serialized.
    fn wire_size(&self) -> usize {
        packet_wire_size(&self.inner)
    }

    /// Serialize to wire format bytes.
    fn serialize<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let bytes = serialize_packet(&self.inner);
        Ok(PyBytes::new(py, &bytes))
    }
}

// ============================================================================
//  Clock (PyClock)
// ============================================================================

/// Clock implementation that delegates to an optional Python callback.
///
/// The callback should be a zero-arg function returning an `int`
/// representing milliseconds.
struct PyClock {
    cb: Option<Py<PyAny>>,
}

impl Clock for PyClock {
    fn now_ms(&self) -> u64 {
        if let Some(ref cb) = self.cb {
            Python::attach(|py| match cb.call0(py) {
                Ok(v) => v.extract::<u64>(py).unwrap_or(0),
                Err(_e) => 0,
            })
        } else {
            0
        }
    }
}

// ============================================================================
//  Router (PyRouter)
// ============================================================================

/// Python-visible router wrapper.
///
/// The underlying `Router` is protected by a `Mutex` for host-side
/// concurrency. Python callbacks for TX, packet handlers, and
/// serialized handlers are kept alive in this object.
#[pyclass(name = "Router")]
pub struct PyRouter {
    // Host-side concurrency: protect the Router with a Mutex and share via Arc.
    inner: SArc<Mutex<Router>>,
    _tx_cb: Option<Py<PyAny>>,
    _pkt_cbs: Vec<Py<PyAny>>,
    _ser_cbs: Vec<Py<PyAny>>,
}

#[pymethods]
impl PyRouter {
    // ------------------------------------------------------------------------
    //  Singleton construction
    // ------------------------------------------------------------------------

    /// Create or retrieve a per-process singleton Router.
    ///
    /// This creates a Router with:
    ///   - no clock (timestamp=0 unless you pass explicit timestamps),
    ///   - no endpoint handlers,
    ///   - an optional TX callback.
    ///
    /// The first call initializes the singleton. Subsequent calls return
    /// another `PyRouter` object wrapping the same underlying Router.
    ///
    /// If you pass a non-None `tx` after the singleton is already created,
    /// an error is raised (the TX callback cannot be changed once set).
    #[staticmethod]
    #[pyo3(signature = (tx=None, now_ms=None, handlers=None))]
    fn new_singleton(
        py: Python<'_>,
        tx: Option<Py<PyAny>>,
        now_ms: Option<Py<PyAny>>,
        handlers: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        // ----------------------------------------------------------
        // 1. If the singleton already exists, return a new wrapper
        // ----------------------------------------------------------
        if let Some(existing) = GLOBAL_ROUTER_SINGLETON.get() {
            // Prevent changing TX / clock / handlers after initialized
            if tx.is_some() || now_ms.is_some() || handlers.is_some() {
                return Err(PyRuntimeError::new_err(
                    "Router singleton already exists; cannot modify tx/now_ms/handlers",
                ));
            }

            return Ok(PyRouter {
                inner: existing.clone(),
                _tx_cb: None,
                _pkt_cbs: Vec::new(),
                _ser_cbs: Vec::new(),
            });
        }

        // ----------------------------------------------------------
        // 2. FIRST CALL — build the whole router (same as __init__)
        // ----------------------------------------------------------

        // Copy callbacks to keep them alive
        let tx_keep = tx.as_ref().map(|p| p.clone_ref(py));
        let now_keep = now_ms.as_ref().map(|p| p.clone_ref(py));

        // Build transmit callback
        let tx_for_closure = tx_keep.as_ref().map(|p| p.clone_ref(py));
        let transmit = if let Some(cb) = tx_for_closure {
            Some(move |bytes: &[u8]| -> TelemetryResult<()> {
                Python::attach(|py| {
                    let arg = PyBytes::new(py, bytes);
                    match cb.call1(py, (&arg,)) {
                        Ok(_) => Ok(()),
                        Err(err) => {
                            err.restore(py);
                            Err(TelemetryError::Io("tx error"))
                        }
                    }
                })
            })
        } else {
            None
        };

        // Build endpoint handlers (same as __init__)
        let mut handlers_vec = Vec::new();
        let mut keep_pkt = Vec::new();
        let mut keep_ser = Vec::new();

        if let Some(hs) = handlers {
            let list = hs.cast::<PyList>().map_err(|_| {
                PyValueError::new_err("handlers must be list of (endpoint, pkt_cb, ser_cb) tuples")
            })?;

            for item in list.iter() {
                let tup = item
                    .cast::<PyTuple>()
                    .map_err(|_| PyValueError::new_err("handler must be a 3-tuple"))?;
                if tup.len() != 3 {
                    return Err(PyValueError::new_err("tuple arity must be 3"));
                }

                let ep_u32: u32 = tup.get_item(0)?.extract()?;
                let endpoint = endpoint_from_u32(ep_u32).map_err(py_err_from)?;

                // Packet handler
                if !tup.get_item(1)?.is_none() {
                    let cb: Py<PyAny> = tup.get_item(1)?.extract()?;
                    let cb_for_closure = cb.clone_ref(py);
                    keep_pkt.push(cb);

                    let eh = EndpointHandler::new_packet_handler(endpoint, move |pkt| {
                        Python::attach(|py| {
                            let py_pkt = PyPacket { inner: pkt.clone() };
                            let any = Py::new(py, py_pkt)
                                .map_err(|_| TelemetryError::Io("packet wrapper"))?;
                            match cb_for_closure.call1(py, (&any,)) {
                                Ok(_) => Ok(()),
                                Err(err) => {
                                    err.restore(py);
                                    Err(TelemetryError::Io("packet handler error"))
                                }
                            }
                        })
                    });

                    handlers_vec.push(eh);
                }

                // Serialized handler
                if !tup.get_item(2)?.is_none() {
                    let cb: Py<PyAny> = tup.get_item(2)?.extract()?;
                    let cb_for_closure = cb.clone_ref(py);
                    keep_ser.push(cb);

                    let eh = EndpointHandler::new_serialized_handler(endpoint, move |bytes| {
                        Python::attach(|py| {
                            let arg = PyBytes::new(py, bytes);
                            match cb_for_closure.call1(py, (&arg,)) {
                                Ok(_) => Ok(()),
                                Err(err) => {
                                    err.restore(py);
                                    Err(TelemetryError::Io("serialized handler error"))
                                }
                            }
                        })
                    });

                    handlers_vec.push(eh);
                }
            }
        }

        // Build clock callback
        let clock = PyClock {
            cb: now_keep.as_ref().map(|p| p.clone_ref(py)),
        };

        // Build router
        let cfg = BoardConfig::new(handlers_vec);
        let router = Router::new(transmit, cfg, Box::new(clock));

        let arc = SArc::new(Mutex::new(router));

        // Store it into OnceLock
        GLOBAL_ROUTER_SINGLETON
            .set(arc.clone())
            .map_err(|_existing| PyRuntimeError::new_err("Router singleton already exists"))?;

        // Return wrapper
        Ok(PyRouter {
            inner: arc,
            _tx_cb: tx_keep,
            _pkt_cbs: keep_pkt,
            _ser_cbs: keep_ser,
        })
    }

    /// Create a new router.
    ///
    /// Parameters
    /// ----------
    /// tx : callable | None
    ///     Called as `tx(bytes: bytes)` for serialized packets.
    /// now_ms : callable | None
    ///     Zero-arg callback returning an integer timestamp in ms.
    /// handlers : list[tuple[int, callable | None, callable | None]] | None
    ///     Each tuple: `(endpoint_id, packet_handler, serialized_handler)`.
    ///     - `packet_handler(pkt: Packet)` is called with a `Packet`.
    ///     - `serialized_handler(bytes: bytes)` is called with raw bytes.
    #[new]
    #[pyo3(signature = (tx=None, now_ms=None, handlers=None))]
    fn new(
        py: Python<'_>,
        tx: Option<Py<PyAny>>,
        now_ms: Option<Py<PyAny>>,
        handlers: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        let tx_keep = tx.as_ref().map(|p| p.clone_ref(py));
        let now_keep = now_ms.as_ref().map(|p| p.clone_ref(py));
        let tx_for_closure = tx_keep.as_ref().map(|p| p.clone_ref(py));

        // Build TX closure
        let transmit = if let Some(cb) = tx_for_closure {
            Some(move |bytes: &[u8]| -> TelemetryResult<()> {
                Python::attach(|py| {
                    let arg = PyBytes::new(py, bytes);
                    match cb.call1(py, (&arg,)) {
                        Ok(_) => Ok(()),
                        Err(err) => {
                            err.restore(py);
                            Err(TelemetryError::Io("tx error"))
                        }
                    }
                })
            })
        } else {
            None
        };

        let mut handlers_vec = Vec::new();
        let mut keep_pkt = Vec::new();
        let mut keep_ser = Vec::new();

        // Build endpoint handlers from Python list/tuples.
        if let Some(hs) = handlers {
            let list = hs.cast::<PyList>().map_err(|_| {
                PyValueError::new_err("handlers must be list of (endpoint, pkt_cb, ser_cb) tuples")
            })?;
            for item in list.iter() {
                let tup = item
                    .cast::<PyTuple>()
                    .map_err(|_| PyValueError::new_err("handler must be a 3-tuple"))?;
                if tup.len() != 3 {
                    return Err(PyValueError::new_err("tuple arity must be 3"));
                }

                let ep_u32: u32 = tup.get_item(0)?.extract()?;
                let endpoint = endpoint_from_u32(ep_u32).map_err(py_err_from)?;

                // Packet handler
                if !tup.get_item(1)?.is_none() {
                    let cb: Py<PyAny> = tup.get_item(1)?.extract()?;
                    let cb_for_closure = cb.clone_ref(py);
                    keep_pkt.push(cb);

                    let eh = EndpointHandler::new_packet_handler(endpoint, move |pkt| {
                        Python::attach(|py| {
                            let py_pkt = PyPacket { inner: pkt.clone() };
                            let any = Py::new(py, py_pkt)
                                .map_err(|_| TelemetryError::Io("packet wrapper"))?;
                            match cb_for_closure.call1(py, (&any,)) {
                                Ok(_) => Ok(()),
                                Err(err) => {
                                    err.restore(py);
                                    Err(TelemetryError::Io("packet handler error"))
                                }
                            }
                        })
                    });
                    handlers_vec.push(eh);
                }

                // Serialized handler
                if !tup.get_item(2)?.is_none() {
                    let cb: Py<PyAny> = tup.get_item(2)?.extract()?;
                    let cb_for_closure = cb.clone_ref(py);
                    keep_ser.push(cb);

                    let eh = EndpointHandler::new_serialized_handler(endpoint, move |bytes| {
                        Python::attach(|py| {
                            let arg = PyBytes::new(py, bytes);
                            match cb_for_closure.call1(py, (&arg,)) {
                                Ok(_) => Ok(()),
                                Err(err) => {
                                    err.restore(py);
                                    Err(TelemetryError::Io("serialized handler error"))
                                }
                            }
                        })
                    });
                    handlers_vec.push(eh);
                }
            }
        }

        let clock = PyClock {
            cb: now_keep.as_ref().map(|p| p.clone_ref(py)),
        };
        let cfg = BoardConfig::new(handlers_vec);
        let router = Router::new(transmit, cfg, Box::new(clock));

        Ok(Self {
            inner: SArc::new(Mutex::new(router)),
            _tx_cb: tx_keep,
            _pkt_cbs: keep_pkt,
            _ser_cbs: keep_ser,
        })
    }

    // ------------------------------------------------------------------------
    //  Logging
    // ------------------------------------------------------------------------

    /// Log raw bytes for a given `DataType`.
    ///
    /// Static-sized payloads are padded/truncated to the exact required length.
    /// Dynamic payloads are passed through verbatim.
    ///
    /// Parameters
    /// ----------
    /// ty : int
    ///     DataType value.
    /// data : bytes | bytearray | memoryview | any buffer-like
    /// timestamp_ms : int | None
    /// queue : bool
    #[pyo3(signature = (ty, data, timestamp_ms=None, queue=false))]
    fn log_bytes(
        &self,
        _py: Python<'_>,
        ty: u32,
        data: &Bound<'_, PyAny>,
        timestamp_ms: Option<u64>,
        queue: bool,
    ) -> PyResult<()> {
        let ty = dtype_from_u32(ty).map_err(py_err_from)?;
        let mut buf: Vec<u8> = data.extract::<&[u8]>()?.to_vec();

        if let Some(required) = required_payload_size_for(ty) {
            if buf.len() < required {
                buf.resize(required, 0u8);
            } else if buf.len() > required {
                buf.truncate(required);
            }
        }

        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        let r = if queue {
            match timestamp_ms {
                Some(ts) => rtr.log_queue_ts::<u8>(ty, ts, &buf),
                None => rtr.log_queue::<u8>(ty, &buf),
            }
        } else {
            match timestamp_ms {
                Some(ts) => rtr.log_ts::<u8>(ty, ts, &buf),
                None => rtr.log::<u8>(ty, &buf),
            }
        };
        r.map_err(py_err_from)
    }

    /// Log an array of `float32` values.
    ///
    /// Static-sized payloads are interpreted as an array of `f32` and
    /// padded/truncated as needed.
    #[pyo3(signature = (ty, values, timestamp_ms=None, queue=false))]
    fn log_f32(
        &self,
        _py: Python<'_>,
        ty: u32,
        values: &Bound<'_, PyAny>,
        timestamp_ms: Option<u64>,
        queue: bool,
    ) -> PyResult<()> {
        let ty = dtype_from_u32(ty).map_err(py_err_from)?;
        let mut vals: Vec<f32> = values.extract()?;

        if let Some(required_bytes) = required_payload_size_for(ty) {
            if required_bytes % 4 != 0 {
                return Err(py_err_from(TelemetryError::BadArg));
            }
            let need = required_bytes / 4;
            if vals.len() < need {
                vals.resize(need, 0.0);
            } else if vals.len() > need {
                vals.truncate(need);
            }
        }

        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        let r = if queue {
            match timestamp_ms {
                Some(ts) => rtr.log_queue_ts::<f32>(ty, ts, &vals),
                None => rtr.log_queue::<f32>(ty, &vals),
            }
        } else {
            match timestamp_ms {
                Some(ts) => rtr.log_ts::<f32>(ty, ts, &vals),
                None => rtr.log::<f32>(ty, &vals),
            }
        };
        r.map_err(py_err_from)
    }

    /// Generic typed logger (C-parity).
    ///
    /// Parameters
    /// ----------
    /// ty : int
    ///     DataType value.
    /// data : bytes | bytearray | memoryview | NumPy array | str | buffer-like
    ///     Any object exposing the buffer protocol (or a Python `str`).
    /// elem_size : int
    ///     Element size in bytes (1, 2, 4, or 8).
    /// elem_kind : ElemKind
    ///     0=UNSIGNED, 1=SIGNED, 2=FLOAT (see `ElemKind` IntEnum).
    /// timestamp_ms : int | None
    /// queue : bool
    #[pyo3(signature = (ty, data, elem_size, elem_kind, timestamp_ms=None, queue=false))]
    fn log(
        &self,
        py: Python<'_>,
        ty: u32,
        data: &Bound<'_, PyAny>,
        elem_size: usize,
        elem_kind: u32,
        timestamp_ms: Option<u64>,
        queue: bool,
    ) -> PyResult<()> {
        if !(elem_size == 1 || elem_size == 2 || elem_size == 4 || elem_size == 8) {
            return Err(PyValueError::new_err("elem_size must be 1,2,4,8"));
        }
        let ty = dtype_from_u32(ty).map_err(py_err_from)?;

        // Robust buffer intake
        let mut bytes: Vec<u8> = if let Ok(b) = data.extract::<&[u8]>() {
            b.to_vec()
        } else if let Ok(py_str) = data.cast::<pyo3::types::PyString>() {
            py_str.to_str()?.as_bytes().to_vec()
        } else {
            let builtins = PyModule::import(py, "builtins")?;
            match builtins.call_method1("bytes", (data.clone(),)) {
                Ok(pybytes) => pybytes.extract::<Vec<u8>>()?,
                Err(_) => {
                    let mv = builtins.getattr("memoryview")?.call1((data.clone(),))?;
                    let itemsize: usize = mv.getattr("itemsize")?.extract()?;
                    let mv_bytes = if itemsize != 1 {
                        mv.call_method1("cast", ("B",))?
                    } else {
                        mv
                    };
                    let pybytes = mv_bytes.call_method0("tobytes")?;
                    pybytes.extract::<Vec<u8>>()?
                }
            }
        };

        // Apply static-size schema, if any.
        if let Some(required) = required_payload_size_for(ty) {
            if bytes.len() < required {
                bytes.resize(required, 0);
            } else if bytes.len() > required {
                bytes.truncate(required);
            }
        }

        let ts = timestamp_ms;

        // Fast path for 1-byte unsigned.
        if elem_size == 1 && elem_kind == EK_UNSIGNED {
            let rtr = self
                .inner
                .lock()
                .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
            let r = if queue {
                match ts {
                    Some(t) => rtr.log_queue_ts::<u8>(ty, t, &bytes),
                    None => rtr.log_queue::<u8>(ty, &bytes),
                }
            } else {
                match ts {
                    Some(t) => rtr.log_ts::<u8>(ty, t, &bytes),
                    None => rtr.log::<u8>(ty, &bytes),
                }
            };
            return r.map_err(py_err_from);
        }

        // Wider elements: reinterpret bytes -> Vec<T> with unaligned LE reads.
        macro_rules! finish_with {
            ($T:ty) => {{
                let cnt = bytes.len() / elem_size;
                if cnt == 0 || bytes.len() % elem_size != 0 {
                    return Err(PyValueError::new_err(
                        "buffer length not divisible by elem_size",
                    ));
                }
                let mut v: Vec<$T> = Vec::with_capacity(cnt);
                vectorize_data::<$T>(bytes.as_ptr(), cnt, elem_size, &mut v)
                    .map_err(|_| PyValueError::new_err("vectorize failed"))?;
                let rtr = self
                    .inner
                    .lock()
                    .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
                let r = if queue {
                    match ts {
                        Some(t) => rtr.log_queue_ts::<$T>(ty, t, &v),
                        None => rtr.log_queue::<$T>(ty, &v),
                    }
                } else {
                    match ts {
                        Some(t) => rtr.log_ts::<$T>(ty, t, &v),
                        None => rtr.log::<$T>(ty, &v),
                    }
                };
                r.map_err(py_err_from)
            }};
        }

        match (elem_kind, elem_size) {
            (EK_UNSIGNED, 2) => finish_with!(u16),
            (EK_UNSIGNED, 4) => finish_with!(u32),
            (EK_UNSIGNED, 8) => finish_with!(u64),

            (EK_SIGNED, 1) => finish_with!(i8),
            (EK_SIGNED, 2) => finish_with!(i16),
            (EK_SIGNED, 4) => finish_with!(i32),
            (EK_SIGNED, 8) => finish_with!(i64),

            (EK_FLOAT, 4) => finish_with!(f32),
            (EK_FLOAT, 8) => finish_with!(f64),

            _ => Err(PyValueError::new_err(
                "unsupported elem_kind/elem_size combination",
            )),
        }
    }

    // ------------------------------------------------------------------------
    //  RX / TX queue plumbing
    // ------------------------------------------------------------------------

    /// Feed serialized packet bytes into the router RX path.
    fn receive_serialized(&self, _py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<()> {
        let bytes: &[u8] = data.extract()?;
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.receive_serialized(bytes).map_err(py_err_from)
    }

    /// Process the router's send/TX queue until empty.
    fn process_send_queue(&self) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.process_tx_queue().map_err(py_err_from)
    }

    /// Process the router's received/RX queue until empty.
    fn process_received_queue(&self) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.process_rx_queue().map_err(py_err_from)
    }

    /// Process both RX and TX queues until empty.
    fn process_all_queues(&self) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.process_all_queues().map_err(py_err_from)
    }

    /// Clear the RX queue (discard any pending received packets).
    fn clear_rx_queue(&self) {
        if let Ok(r) = self.inner.lock() {
            r.clear_rx_queue();
        }
    }

    /// Clear the TX queue (discard any pending transmissions).
    fn clear_tx_queue(&self) {
        if let Ok(r) = self.inner.lock() {
            r.clear_tx_queue();
        }
    }

    /// Clear both RX and TX queues.
    fn clear_queues(&self) {
        if let Ok(r) = self.inner.lock() {
            r.clear_queues();
        }
    }

    // ------------------------------------------------------------------------
    //  Time-budgeted variants
    // ------------------------------------------------------------------------

    /// Process the TX queue, stopping after `timeout_ms` milliseconds
    /// or when the queue becomes empty.
    fn process_tx_queue_with_timeout(&self, timeout_ms: u32) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.process_tx_queue_with_timeout(timeout_ms)
            .map_err(py_err_from)
    }

    /// Process the RX queue, stopping after `timeout_ms` milliseconds
    /// or when the queue becomes empty.
    fn process_rx_queue_with_timeout(&self, timeout_ms: u32) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.process_rx_queue_with_timeout(timeout_ms)
            .map_err(py_err_from)
    }

    /// Process both RX and TX queues, stopping after `timeout_ms` milliseconds
    /// or when both queues are empty.
    fn process_all_queues_with_timeout(&self, timeout_ms: u32) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.process_all_queues_with_timeout(timeout_ms)
            .map_err(py_err_from)
    }
}

// ============================================================================
//  Top-level helper functions
// ============================================================================

/// Deserialize raw packet bytes into a `Packet` instance.
///
/// Raises a `RuntimeError` if deserialization or validation fails.
#[pyfunction]
pub fn deserialize_packet_py(py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let bytes: &[u8] = data.extract()?;
    let pkt = deserialize_packet(bytes).map_err(py_err_from)?;
    if let Err(e) = pkt.validate() {
        return Err(py_err_from(e));
    }
    Ok(Py::new(py, PyPacket { inner: pkt })?.into_any())
}

/// Peek only the header fields of a serialized packet.
///
/// Returns a dict with:
/// - "ty"
/// - "sender"
/// - "endpoints"
/// - "timestamp_ms"
#[pyfunction]
pub fn peek_header_py(py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let bytes: &[u8] = data.extract()?;
    let env = peek_envelope(bytes).map_err(py_err_from)?;

    let out = PyDict::new(py);
    out.set_item("ty", env.ty as u32)?;
    out.set_item("sender", env.sender.as_ref())?;
    out.set_item(
        "endpoints",
        env.endpoints
            .iter()
            .map(|e| *e as u32)
            .collect::<Vec<u32>>(),
    )?;
    out.set_item("timestamp_ms", env.timestamp_ms)?;
    Ok(out.unbind().into_any())
}

/// Construct a `Packet` from explicit fields.
///
/// Static-sized payloads will be padded/truncated to the exact required length.
/// Dynamic payloads will be passed through verbatim.
#[pyfunction]
#[pyo3(signature = (ty, sender, endpoints, timestamp_ms, payload))]
pub fn make_packet(
    py: Python<'_>,
    ty: u32,
    sender: &str,
    endpoints: Vec<u32>,
    timestamp_ms: u64,
    payload: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let ty = dtype_from_u32(ty).map_err(py_err_from)?;

    // Convert endpoint IDs → DataEndpoint
    let eps: Vec<DataEndpoint> = endpoints
        .into_iter()
        .map(|e| endpoint_from_u32(e).map_err(py_err_from))
        .collect::<Result<_, _>>()?;

    // Extract payload from Python (buffer protocol)
    let mut buf: Vec<u8> = payload.extract()?;

    // Static-sized: enforce exact length; Dynamic: pass-through
    if let Some(required) = required_payload_size_for(ty) {
        if buf.len() < required {
            buf.resize(required, 0u8);
        } else if buf.len() > required {
            buf.truncate(required);
        }
    }

    let payload_arc = AArc::<[u8]>::from(buf);

    // Go through TelemetryPacket::new for validation + consistency
    let pkt = TelemetryPacket::new(
        ty,
        &eps,
        AArc::<str>::from(sender),
        timestamp_ms,
        payload_arc,
    )
    .map_err(py_err_from)?;

    Ok(Py::new(py, PyPacket { inner: pkt })?.into_any())
}

// ============================================================================
//  Module init: classes, functions, dynamic enums
// ============================================================================

/// Python module entry point.
///
/// The module name is `sedsprintf_rs` on the Python side.
#[pymodule]
pub fn sedsprintf_rs_2026(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    // === Classes ===
    m.add_class::<PyRouter>()?;
    m.add_class::<PyPacket>()?;

    // === Functions ===
    m.add_function(wrap_pyfunction!(deserialize_packet_py, m)?)?;
    m.add_function(wrap_pyfunction!(peek_header_py, m)?)?;
    m.add_function(wrap_pyfunction!(make_packet, m)?)?;

    // === Dynamic Enum Creation ===
    let enum_mod = PyModule::import(py, "enum")?;
    let int_enum = enum_mod.getattr("IntEnum")?;

    // Get the real module name to stamp on the classes
    let mod_name: String = m.getattr("__name__")?.extract()?;

    // ------------------ DataType ------------------
    {
        let dt_dict = PyDict::new(py);
        dt_dict.set_item("__module__", &mod_name)?;

        for v in 0..=MAX_VALUE_DATA_TYPE {
            if let Some(e) = try_enum_from_u32::<DataType>(v) {
                let name = e.as_str();
                dt_dict.set_item(name, v)?;
                m.add(name, v)?; // convenience constants
            }
        }
        let dt_enum = int_enum.call1(("DataType", dt_dict))?;
        m.add("DataType", dt_enum)?;
    }

    // ------------------ DataEndpoint ------------------
    {
        let ep_dict = PyDict::new(py);
        ep_dict.set_item("__module__", &mod_name)?;
        for v in 0..=MAX_VALUE_DATA_ENDPOINT {
            if let Some(e) = try_enum_from_u32::<DataEndpoint>(v) {
                let name = e.as_str();
                ep_dict.set_item(name, v)?;
                m.add(name, v)?;
            }
        }
        let ep_enum = int_enum.call1(("DataEndpoint", ep_dict))?;
        m.add("DataEndpoint", ep_enum)?;
    }

    // ------------------ ElemKind ------------------
    {
        let ek_dict = PyDict::new(py);
        ek_dict.set_item("__module__", &mod_name)?;
        ek_dict.set_item("UNSIGNED", EK_UNSIGNED)?;
        ek_dict.set_item("SIGNED", EK_SIGNED)?;
        ek_dict.set_item("FLOAT", EK_FLOAT)?;
        let ek_enum = int_enum.call1(("ElemKind", ek_dict))?;
        m.add("ElemKind", ek_enum)?;
    }

    Ok(())
}
