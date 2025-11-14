import sedsprintf_rs_2026 as seds
import multiprocessing as mp
import time

DT = seds.DataType
EP = seds.DataEndpoint
EK = seds.ElemKind
# ---------------- Enum helpers ----------------

global_packets: list[seds.Packet] = []

def enum_to_int(obj):
    try:
        return int(obj)
    except Exception:
        return obj

def _load_packet_to_database(data: seds.Packet):
    global global_packets
    global_packets.append(data)

def _tx(_bytes_buf: bytes):
    # Transmission stub (no-op)
    pass

def _now_ms() -> int:
    return int(time.time())

def create_router():
    """
    Create a SEDS router with default handlers if none are provided.
    """
    handlers = [
        (int(EP.GROUND_STATION), _load_packet_to_database, None),
    ]
    router = seds.Router.new_singleton(tx=_tx, now_ms=_now_ms, handlers=handlers)
    return router


def __process_queue_loop( timeout_ms: int):
    router = seds.Router.new_singleton(tx=None, now_ms=None, handlers=None)
    while True:
        router.process_all_queues_with_timeout(timeout_ms)

def spawn_async_queue_processor(timeout_ms: int):
    thread = mp.Process(target=__process_queue_loop, args=(timeout_ms,),
                           daemon=True)

    thread.start()
    return thread
