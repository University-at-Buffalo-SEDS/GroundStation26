# Find a station on the local network

On the native connection setup screen, choose **Discover stations**, select a GroundStation or RRLS, then choose **Connect**. Manual host entry and connection diagnostics remain available. Discovery searches IPv4 broadcast interfaces for 1.5 seconds and lists up to 16 distinct addresses.

The station backend must include the public `GS26-FIND1` responder on UDP port 49363. It advertises its active role and actual HTTP/HTTPS listener port; HTTPS is preferred when enabled. This works before sign-in and without station pairing, internet, or a central discovery service. Firewalls, client isolation, VPNs, separate subnets, and mobile local-network permissions can prevent replies. When no replies arrive, enter an address manually.

Discovery results are untrusted address hints. The scan sends no credentials and does not install certificates, bypass certificate checks, or establish paired failover trust. Selection fills the existing address fields; it does not connect automatically. Normal TLS and sign-in checks run when the user connects. The separate signed `GS26-STATION1` paired handover protocol remains unchanged.

Browser dashboards use the origin that served the page and do not have the native connection screen. Browser applications cannot send this UDP LAN broadcast. Open a station's URL or use the native application for first-use LAN discovery. Existing browser paired-origin failover remains available independently.
