#[cfg(test)]
mod threaded_system_tests {
    use sedsprintf_rs_2026::config::{DataEndpoint, DataType};
    use sedsprintf_rs_2026::router::{BoardConfig, Clock, EndpointHandler, Router};
    use sedsprintf_rs_2026::telemetry_packet::TelemetryPacket;
    use sedsprintf_rs_2026::TelemetryResult;

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};


    fn zero_clock() -> Box<dyn Clock + Send + Sync> {
        Box::new(|| 0u64)
    }

    /// Simulated node in the Rust system test.
    struct SimNode {
        router: Arc<Router>,
        radio_hits: Arc<AtomicUsize>,
        sd_hits: Arc<AtomicUsize>,
    }

    /// Build a handler that counts packets received on the GroundStation endpoint
    /// (this plays the role of the "radio" handler in the C test).
    fn make_radio_handler(counter: Arc<AtomicUsize>) -> EndpointHandler {
        EndpointHandler::new_packet_handler(DataEndpoint::GroundStation, move |_pkt: &TelemetryPacket| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    /// Build a handler that counts packets received on the SdCard endpoint
    /// (this plays the role of the "SD" handler in the C test).
    fn make_sd_handler(counter: Arc<AtomicUsize>) -> EndpointHandler {
        EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt: &TelemetryPacket| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    /// Helper to generate a simple float series like the C helper `make_series`.
    fn make_series(buf: &mut [f32], base: f32) {
        for (i, v) in buf.iter_mut().enumerate() {
            *v = base + (i as f32) * 0.25;
        }
    }

    /// Build a packet with endpoints [SD_CARD, Radio], mirroring the C
    /// system’s idea that every message goes to both "radio" and "SD".
    fn make_packet(ty: DataType, vals: &[f32], ts: u64) -> TelemetryPacket {
        TelemetryPacket::from_f32_slice(
            ty,
            vals,
            &[DataEndpoint::SdCard, DataEndpoint::GroundStation],
            ts,
        )
        .unwrap()
    }

    /// Threaded system test that mirrors `main.c` but uses the Rust
    /// router/packet library directly.
    #[test]
    fn threaded_system_sim_rust() {
        // ------------- 1) Shared bus -------------
        // The bus is a simple MPSC channel of (from_node_idx, wire_bytes).
        type BusMsg = (usize, Vec<u8>);
        let (bus_tx, bus_rx) = mpsc::channel::<BusMsg>();

        // ------------- 2) Build nodes -------------
        let stop_flag = Arc::new(AtomicBool::new(false));

        // Node 0: "Radio Board" (radio_hits via GroundStation endpoint)
        // Node 1: "Flight Controller Board" (sd_hits via SdCard endpoint)
        // Node 2: "Power Board" (no local endpoints)
        let mut nodes: Vec<SimNode> = Vec::new();

        for (idx, _) in ["Radio Board", "Flight Controller Board", "Power Board"]
            .iter()
            .enumerate()
        {
            let radio_hits = Arc::new(AtomicUsize::new(0));
            let sd_hits = Arc::new(AtomicUsize::new(0));

            let mut handlers = Vec::<EndpointHandler>::new();
            match idx {
                // Radio Board: only "radio" (GroundStation) is local
                0 => {
                    handlers.push(make_radio_handler(radio_hits.clone()));
                }
                // Flight Controller Board: only "SD card" is local
                1 => {
                    handlers.push(make_sd_handler(sd_hits.clone()));
                }
                // Power Board: no local endpoints
                _ => {}
            }

            let clock = zero_clock();
            let tx = {
                let bus_tx = bus_tx.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    // push a copy of the wire bytes onto the bus with source id
                    bus_tx.send((idx, bytes.to_vec())).unwrap();
                    Ok(())
                }
            };

            let router = if handlers.is_empty() {
                Router::new::<_>(Some(tx), BoardConfig::default(), clock)
            } else {
                Router::new(Some(tx), BoardConfig::new(handlers), clock)
            };

            nodes.push(SimNode {
                router: Arc::new(router),
                radio_hits,
                sd_hits,
            });
        }

        let nodes = Arc::new(nodes);

        // ------------- 3) Bus thread -------------
        // Broadcast every frame from one node to all *other* nodes.
        let bus_nodes = nodes.clone();
        let bus_stop = stop_flag.clone();
        let bus_handle = thread::spawn(move || {
            while !bus_stop.load(Ordering::SeqCst) {
                match bus_rx.recv_timeout(Duration::from_millis(10)) {
                    Ok((from, frame)) => {
                        for (idx, node) in bus_nodes.iter().enumerate() {
                            if idx == from {
                                continue; // don't loop back to sender via bus
                            }
                            node.router
                                .rx_serialized_packet_to_queue(&frame)
                                .expect("rx_serialized_packet_to_queue failed");
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        // nothing pending, loop again
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }

            // Final drain of any remaining frames (non-blocking)
            while let Ok((from, frame)) = bus_rx.try_recv() {
                for (idx, node) in bus_nodes.iter().enumerate() {
                    if idx == from {
                        continue;
                    }
                    node.router
                        .rx_serialized_packet_to_queue(&frame)
                        .expect("rx_serialized_packet_to_queue failed during final drain");
                }
            }
        });

        // ------------- 4) Processor threads (one per node) -------------
        let mut proc_handles = Vec::new();
        for node in nodes.iter() {
            let r = node.router.clone();
            let stop = stop_flag.clone();
            let handle = thread::spawn(move || {
                // mimic the C processor_thread: repeatedly process TX + RX with small timeouts
                while !stop.load(Ordering::SeqCst) {
                    r.process_all_queues_with_timeout(5).unwrap();
                    thread::sleep(Duration::from_millis(1));
                }

                // Final drain
                for _ in 0..50 {
                    r.process_all_queues_with_timeout(0).unwrap();
                    thread::sleep(Duration::from_millis(1));
                }
            });
            proc_handles.push(handle);
        }

        // ------------- 5) Sender threads (A, B, C) -------------
        // We follow the same structure as the C senders:
        // - A: logs GPS_DATA 5 times
        // - B: logs IMU_DATA and BAROMETER_DATA 5 times each loop
        // - C: logs BATTERY_STATUS (mapped here to BatteryVoltage) and MESSAGE_DATA
        //       (mapped here to TelemetryError as a string type)

        let radio_router = nodes[0].router.clone();
        let flight_router = nodes[1].router.clone();
        let power_router = nodes[2].router.clone();

        // Sender A
        let sender_a = thread::spawn(move || {
            let mut buf = [0.0_f32; 8];
            for i in 0..5 {
                make_series(&mut buf[..2], 10.0);
                let pkt = make_packet(DataType::GpsData, &buf[..2], i);
                radio_router.transmit_message(&pkt).unwrap();
                thread::sleep(Duration::from_millis(5));
            }
        });

        // Sender B
        let sender_b = thread::spawn(move || {
            let mut buf = [0.0_f32; 8];
            for i in 0..5 {
                // IMU-like data
                make_series(&mut buf[..2], 0.5);
                let pkt1 = make_packet(DataType::GpsData, &buf[..2], i);
                // If you have a dedicated IMU type, swap DataType::GpsData for it.

                flight_router.transmit_message(&pkt1).unwrap();
                thread::sleep(Duration::from_millis(5));

                // BARO-like data
                make_series(&mut buf[..2], 101.3);
                let pkt2 = make_packet(DataType::GpsData, &buf[..2], i + 100);
                // If you have a dedicated barometer type, swap DataType::GpsData for it.

                flight_router.transmit_message(&pkt2).unwrap();
                thread::sleep(Duration::from_millis(5));
            }
        });

        // Sender C
        let sender_c = thread::spawn(move || {
            let mut buf = [0.0_f32; 8];
            for i in 0..5 {
                make_series(&mut buf[..1], 3.7);
                let pkt1 = make_packet(DataType::BatteryVoltage, &buf[..1], i + 200);
                power_router.transmit_message(&pkt1).unwrap();
                thread::sleep(Duration::from_millis(5));

                // Message-like data → use TelemetryError as a String-typed payload
                let msg = "hello world!";
                let pkt2 = TelemetryPacket::from_u8_slice(
                    DataType::TelemetryError,
                    msg.as_bytes(),
                    &[DataEndpoint::SdCard, DataEndpoint::GroundStation],
                    i + 300,
                )
                .unwrap();
                // Send via router so it goes through TX path and bus.
                power_router.transmit_message(&pkt2).unwrap();
                thread::sleep(Duration::from_millis(5));
            }
        });

        // ------------- 6) Join senders -------------
        sender_a.join().expect("sender A panicked");
        sender_b.join().expect("sender B panicked");
        sender_c.join().expect("sender C panicked");

        // ------------- 7) Wait for expected hits or timeout -------------
        let expected_total = 25; // 5 + 2*5 + 2*5 = 25 logs total, like C example
        let deadline = Instant::now() + Duration::from_secs(5);

        loop {
            let a_radio = nodes[0].radio_hits.load(Ordering::SeqCst);
            let b_sd = nodes[1].sd_hits.load(Ordering::SeqCst);

            if a_radio == expected_total && b_sd == expected_total {
                break;
            }

            if Instant::now() > deadline {
                eprintln!(
                    "Timeout waiting for processors to drain queues: A.radio_hits={}, B.sd_hits={}",
                    a_radio, b_sd
                );
                break;
            }

            thread::sleep(Duration::from_millis(10));
        }

        // ------------- 8) Stop processors + bus and join -------------
        stop_flag.store(true, Ordering::SeqCst);

        for handle in proc_handles {
            handle.join().expect("processor thread panicked");
        }
        bus_handle.join().expect("bus thread panicked");

        // ------------- 9) Assertions (mirror C system test) -------------
        let radio_board = &nodes[0];
        let flight_board = &nodes[1];
        let power_board = &nodes[2];

        println!(
            "A.radio_hits={}, B.sd_hits={}, C.radio_hits={}, C.sd_hits={}",
            radio_board.radio_hits.load(Ordering::SeqCst),
            flight_board.sd_hits.load(Ordering::SeqCst),
            power_board.radio_hits.load(Ordering::SeqCst),
            power_board.sd_hits.load(Ordering::SeqCst),
        );

        assert_eq!(
            radio_board.radio_hits.load(Ordering::SeqCst),
            expected_total,
            "Radio Board should see {} packets",
            expected_total
        );
        assert_eq!(
            flight_board.sd_hits.load(Ordering::SeqCst),
            expected_total,
            "Flight Controller Board should see {} SD packets",
            expected_total
        );
        assert_eq!(
            power_board.radio_hits.load(Ordering::SeqCst),
            0,
            "Power Board must not have a radio handler"
        );
        assert_eq!(
            power_board.sd_hits.load(Ordering::SeqCst),
            0,
            "Power Board must not have an SD handler"
        );
    }
}
