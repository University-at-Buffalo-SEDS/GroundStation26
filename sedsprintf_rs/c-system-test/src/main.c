// threaded_sim_main.c
// Build (Linux/macOS): cc -O2 -pthread threaded_sim_main.c telemetry_sim_threadsafe.c -o sim
// Requires your sedsprintf + telemetry_sim headers/libs available in include/lib paths.

#include <assert.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/time.h>
#include <unistd.h>

#include "telemetry_sim.h"
#include "sedsprintf.h"

// --------- helpers ----------
static void make_series(float * out, size_t n, float base)
{
    for (size_t i = 0; i < n; ++i) out[i] = base + (float) i * 0.25f;
}

static uint64_t gen_random_us(void)
{
    const uint32_t min_ms = 20, max_ms = 1000;
    uint32_t v = 0;
    int fd = open("/dev/urandom", O_RDONLY);
    if (fd >= 0)
    {
        if (read(fd, &v, sizeof(v)) != sizeof(v)) v = 0;
        close(fd);
    }
    uint32_t ms = (v % (max_ms - min_ms + 1)) + min_ms;
    return (uint64_t) ms * 1000ULL;
}

// --------- global stop flag ----------
static volatile int g_stop = 0;

// --------- processor thread (per-node) ----------
static void * processor_thread(void * arg)
{
    SimNode * n = (SimNode *) arg;
    while (!g_stop)
    {
        // Small timeouts so we can respond to g_stop quickly
        seds_router_process_tx_queue_with_timeout(n->r, 5);
        seds_router_process_rx_queue_with_timeout(n->r, 5);
        // Be nice to the scheduler
        usleep(1000);
    }
    // Final drain to avoid stragglers
    for (int i = 0; i < 50; ++i)
    {
        seds_router_process_tx_queue_with_timeout(n->r, 0);
        seds_router_process_rx_queue_with_timeout(n->r, 0);
        usleep(1000);
    }
    return NULL;
}

// --------- sender threads (one per node) ----------
typedef struct
{
    SimNode * nodeA; // Radio Board (RADIO handler)
    SimNode * nodeB; // Flight Controller (SD handler)
    SimNode * nodeC; // Power Board (no local endpoints)
} Topo;

static void * sender_A(void * arg)
{
    (void) arg;
    SimNode * A = ((Topo *) arg)->nodeA;
    float buf[8];
    for (int i = 0; i < 5; ++i)
    {
        make_series(buf, 3, 10.0f);
        assert(node_log(A, SEDS_DT_GPS_DATA, buf, 3, sizeof(buf[0])) == SEDS_OK);
        usleep(gen_random_us());
    }
    return NULL;
}

static void * sender_B(void * arg)
{
    SimNode * B = ((Topo *) arg)->nodeB;
    float buf[8];
    for (int i = 0; i < 5; ++i)
    {
        make_series(buf, 3, 0.5f);
        assert(node_log(B, SEDS_DT_GYRO_DATA, buf, 3, sizeof(buf[0])) == SEDS_OK);
        usleep(gen_random_us());

        make_series(buf, 3, 101.3f);
        assert(node_log(B, SEDS_DT_BAROMETER_DATA, buf, 3, sizeof(buf[0])) == SEDS_OK);
        usleep(gen_random_us());
    }
    return NULL;
}

static void * sender_C(void * arg)
{
    SimNode * C = ((Topo *) arg)->nodeC;
    float buf[8];
    for (int i = 0; i < 5; ++i)
    {
        make_series(buf, 1, 3.7f);
        assert(node_log(C, SEDS_DT_BATTERY_VOLTAGE, buf, 1, sizeof(buf[0])) == SEDS_OK);
        usleep(gen_random_us());

        const char * msg = "hello world!";
        assert(node_log(C, SEDS_DT_MESSAGE_DATA, msg, strlen(msg), sizeof(msg[0])) == SEDS_OK);
        usleep(gen_random_us());
    }
    return NULL;
}

int main(void)
{
    // 1) Bus + nodes
    SimBus bus;
    bus_init(&bus);

    SimNode radioBoard, flightControllerBoard, powerBoard;
    assert(node_init(&radioBoard, &bus, "Radio Board", 1, 0) == SEDS_OK);
    assert(node_init(&flightControllerBoard, &bus, "Flight Controller Board", 0, 1) == SEDS_OK);
    assert(node_init(&powerBoard, &bus, "Power Board", 0, 0) == SEDS_OK);

    // 2) Processor threads (one per node)
    pthread_t procA, procB, procC;
    pthread_create(&procA, NULL, processor_thread, &radioBoard);
    pthread_create(&procB, NULL, processor_thread, &flightControllerBoard);
    pthread_create(&procC, NULL, processor_thread, &powerBoard);

    // 3) Sender threads (A, B, C)
    Topo topo = {.nodeA = &radioBoard, .nodeB = &flightControllerBoard, .nodeC = &powerBoard};
    pthread_t thA, thB, thC;
    pthread_create(&thA, NULL, sender_A, &topo);
    pthread_create(&thB, NULL, sender_B, &topo);
    pthread_create(&thC, NULL, sender_C, &topo);

    // 4) Wait for senders
    pthread_join(thA, NULL);
    pthread_join(thB, NULL);
    pthread_join(thC, NULL);

    // 5) Wait until expected hits arrive or timeout
    //    We expect 25 packets total → Radio handler should see 25; SD handler should see 25.
    const uint64_t deadline_us = 5ULL * 1000 * 1000; // 5s safety
    struct timeval start, now;
    gettimeofday(&start, NULL);

    for (;;)
    {
        if (radioBoard.radio_hits == 25 && flightControllerBoard.sd_hits == 25) break;

        gettimeofday(&now, NULL);
        uint64_t elapsed_us =
                (uint64_t) (now.tv_sec - start.tv_sec) * 1000000ULL +
                (uint64_t) (now.tv_usec - start.tv_usec);
        if (elapsed_us > deadline_us)
        {
            fprintf(stderr, "Timeout waiting for processors to drain queues\n");
            break;
        }
        // nudge processors
        usleep(1000);
    }

    // 6) Stop processors and join
    g_stop = 1;
    pthread_join(procA, NULL);
    pthread_join(procB, NULL);
    pthread_join(procC, NULL);

    printf("A.radio_hits=%u, B.sd_hits=%u, C.radio_hits=%u, C.sd_hits=%u\n",
           radioBoard.radio_hits, flightControllerBoard.sd_hits,
           powerBoard.radio_hits, powerBoard.sd_hits);

    // 7) Assertions (same as your single-thread example)
    assert(radioBoard.radio_hits == 25);
    assert(flightControllerBoard.sd_hits == 25);
    assert(powerBoard.radio_hits == 0);
    assert(powerBoard.sd_hits == 0);

    // 8) Cleanup
    node_free(&radioBoard);
    node_free(&flightControllerBoard);
    node_free(&powerBoard);
    bus_free(&bus);
    return 0;
}
