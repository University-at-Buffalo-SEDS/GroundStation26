#include "telemetry.h"
#include "sedsprintf.h"


int main(void)
{
    //synchronous code
    init_telemetry_router();
    const float data[3] = {37.7749f, -122.4194f, 30.0f};
    SedsResult result = log_telemetry_synchronous(SEDS_DT_GPS_DATA, data, 3, sizeof(data[0]));
    if (result != SEDS_OK)
    {
        print_handle_telemetry_error(result);
    }
    //asynchronous code
    //this would be in send routine of the data collector
    result = log_telemetry_asynchronous(SEDS_DT_GPS_DATA, data, 3, sizeof(data[0]));
    if (result != SEDS_OK)
    {
        print_handle_telemetry_error(result);
    }
    //this would be in the main loop of the program or in a freertos task.
    result = process_all_queues_timeout(20);
    if (result != SEDS_OK)
    {
        print_handle_telemetry_error(result);
    }
    // dispatch_tx_queue_timeout(100);

    //this would be in the isr of the receiver
    //rx_asynchronous(received_bytes, received_length);

    //


    return 0;
}
