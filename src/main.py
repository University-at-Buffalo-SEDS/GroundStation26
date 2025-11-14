import telemetry.telemetry as telemetry


def main():
    router = telemetry.create_router()
    thread = telemetry.spawn_async_queue_processor(timeout_ms=0)

    thread.join()

if __name__ == "__main__":
    main()