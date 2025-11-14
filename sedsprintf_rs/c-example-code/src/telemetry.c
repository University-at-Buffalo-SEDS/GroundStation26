// telemetry_host_threadsafe.c
#include "telemetry.h"
#include "sedsprintf.h"

#include <stdio.h>
#include <stdint.h>
#include <time.h>
#include <sys/time.h>

/* ---------------- Monotonic ms provider ---------------- */
static uint64_t host_now_ms(void * user)
{
    (void) user;
#ifdef _WIN32
    static LARGE_INTEGER freq = {0};
    LARGE_INTEGER ctr;
    if (freq.QuadPart == 0) { QueryPerformanceFrequency(&freq); }
    QueryPerformanceCounter(&ctr);
    return (uint64_t) ((ctr.QuadPart * 1000ULL) / (uint64_t) freq.QuadPart);
#else
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return (uint64_t) tv.tv_sec * 1000ULL + (uint64_t) (tv.tv_usec / 1000ULL);
#endif
}

/* ---------------- Global router state ---------------- */
RouterState g_router = {.r = NULL, .created = 0, .start_time = 0};


/* ---------------- TX: stub ---------------- */
SedsResult tx_send(const uint8_t * bytes, size_t len, void * user)
{
    (void) user;
    (void) bytes;
    (void) len;
    return SEDS_OK;
}

/* ---------------- RX helpers ---------------- */
void rx_synchronous(const uint8_t * bytes, size_t len)
{
    if (!bytes || len == 0) return;

    if (!g_router.r) { if (init_telemetry_router() != SEDS_OK) return; }

    seds_router_receive_serialized(g_router.r, bytes, len);
}

void rx_asynchronous(const uint8_t * bytes, size_t len)
{
    if (!bytes || len == 0) return;

    if (!g_router.r) { if (init_telemetry_router() != SEDS_OK) return; }

    seds_router_rx_serialized_packet_to_queue(g_router.r, bytes, len);
}

/* ---------------- Local endpoint handler ---------------- */
SedsResult on_radio_packet(const SedsPacketView * pkt, void * user)
{
    (void) user;
    char buf[seds_pkt_to_string_len(pkt)];
    SedsResult s = seds_pkt_to_string(pkt, buf, sizeof(buf));
    if (s != SEDS_OK)
    {
        printf("on_radio_packet: seds_pkt_to_string failed: %d\n", s);
        return s;
    }
    printf("on_radio_packet: %s\n", buf);
    return SEDS_OK;
}

/* ---------------- Router init (idempotent + locked) ---------------- */
SedsResult init_telemetry_router(void)
{
    /* Fast path without lock */
    if (g_router.created && g_router.r) return SEDS_OK;


    if (g_router.created && g_router.r) { return SEDS_OK; }

    const SedsLocalEndpointDesc locals[] = {
        {.endpoint = SEDS_EP_RADIO, .packet_handler = on_radio_packet, .user = NULL},
    };

    SedsRouter * r = seds_router_new(
        tx_send,
        NULL, /* tx_user */
        host_now_ms, /* clock */
        locals,
        sizeof(locals) / sizeof(locals[0])
    );

    if (!r)
    {
        printf("Error: failed to create router\n");
        g_router.r = NULL;
        g_router.created = 0;

        return SEDS_ERR;
    }

    g_router.r = r;
    g_router.created = 1;
    g_router.start_time = host_now_ms(NULL);


    return SEDS_OK;
}

/* ---------------- Logging APIs (locked) ---------------- */
SedsResult log_telemetry_synchronous(SedsDataType data_type,
                                     const void * data,
                                     size_t element_count,
                                     size_t element_size)
{
    if (!data || element_count == 0 || element_size == 0) return SEDS_ERR;

    if (!g_router.r) { if (init_telemetry_router() != SEDS_OK) return SEDS_ERR; }

    const size_t total_bytes = element_count * element_size;

    const SedsResult res = seds_router_log(g_router.r, data_type, data, total_bytes);

    return res;
}

SedsResult log_telemetry_asynchronous(SedsDataType data_type,
                                      const void * data,
                                      size_t element_count,
                                      size_t element_size)
{
    if (!data || element_count == 0 || element_size == 0) return SEDS_ERR;

    if (!g_router.r) { if (init_telemetry_router() != SEDS_OK) return SEDS_ERR; }

    const size_t total_bytes = element_count * element_size;

    const SedsResult res = seds_router_log_queue(g_router.r, data_type, data, total_bytes);

    return res;
}

/* ---------------- Queue processing (locked) ---------------- */
SedsResult dispatch_tx_queue(void)
{
    if (!g_router.r) { if (init_telemetry_router() != SEDS_OK) return SEDS_ERR; }

    const SedsResult res = seds_router_process_tx_queue(g_router.r);

    return res;
}

SedsResult process_rx_queue(void)
{
    if (!g_router.r) { if (init_telemetry_router() != SEDS_OK) return SEDS_ERR; }

    const SedsResult res = seds_router_process_rx_queue(g_router.r);

    return res;
}

SedsResult dispatch_tx_queue_timeout(uint32_t timeout_ms)
{
    if (!g_router.r) { if (init_telemetry_router() != SEDS_OK) return SEDS_ERR; }

    const SedsResult res = seds_router_process_tx_queue_with_timeout(g_router.r, timeout_ms);

    return res;
}

SedsResult process_rx_queue_timeout(uint32_t timeout_ms)
{
    if (!g_router.r) { if (init_telemetry_router() != SEDS_OK) return SEDS_ERR; }

    const SedsResult res = seds_router_process_rx_queue_with_timeout(g_router.r, timeout_ms);

    return res;
}

SedsResult process_all_queues_timeout(uint32_t timeout_ms)
{
    if (!g_router.r) { if (init_telemetry_router() != SEDS_OK) return SEDS_ERR; }

    const SedsResult res = seds_router_process_all_queues_with_timeout(g_router.r, timeout_ms);

    return res;
}

/* ---------------- Error printing ---------------- */
SedsResult print_handle_telemetry_error(const int32_t error_code)
{
    char buf[seds_error_to_string_len(error_code)];
    const SedsResult res = seds_error_to_string(error_code, buf, sizeof(buf));
    if (res != SEDS_OK)
    {
        printf("handle_error: seds_error_to_string failed: %d\n", res);
        return res;
    }
    printf("Error: %s\n", buf);
    return SEDS_OK;
}
