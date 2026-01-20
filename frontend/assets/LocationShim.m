// LocationShim.m
//
// iOS/macOS Objective-C shim for:
//  - Requesting Location permission + streaming coordinates to a Rust callback
//  - Triggering the Local Network permission prompt by attempting a short-lived
//    LAN TCP connection (requires NSLocalNetworkUsageDescription in Info.plist)
//
// Build notes:
//  - Compile with ARC: -fobjc-arc
//  - Link frameworks: CoreLocation.framework, Network.framework
//

#import <Foundation/Foundation.h>
#import <CoreLocation/CoreLocation.h>
#import <Network/Network.h>

#pragma mark - Local Network permission poke

// Keep the connection alive briefly; otherwise it may be deallocated immediately.
static nw_connection_t g_local_net_conn = NULL;

void gs26_request_local_network_prompt(void) {
    @autoreleasepool {
        // Don't spam attempts; one is enough to trigger the prompt.
        if (g_local_net_conn != NULL) {
            return;
        }

        // Try a common local gateway. You can swap this for your ground station IP/port.
        // Using an RFC1918 address is what typically triggers the Local Network prompt.
        nw_endpoint_t ep = nw_endpoint_create_host("192.168.1.1", "80");

        // Create TCP parameters without TLS.
        // Signature: nw_parameters_create_secure_tcp(tls_parameters, tcp_parameters)
        // Passing NULL for tls_parameters yields plain TCP.
        nw_parameters_t params = nw_parameters_create_secure_tcp(NULL, NULL);

        g_local_net_conn = nw_connection_create(ep, params);

        dispatch_queue_t q = dispatch_get_global_queue(QOS_CLASS_UTILITY, 0);
        nw_connection_set_queue(g_local_net_conn, q);

        // Optional: track state changes for debugging (leave silent in production).
        nw_connection_set_state_changed_handler(g_local_net_conn, ^(nw_connection_state_t state, nw_error_t error) {
            (void)state;
            (void)error;
        });

        // Start the connection attempt. Success is NOT required.
        nw_connection_start(g_local_net_conn);

        // Cancel shortly after; we only want the attempt to force the permission prompt.
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(300 * NSEC_PER_MSEC)), q, ^{
            if (g_local_net_conn != NULL) {
                nw_connection_cancel(g_local_net_conn);
                g_local_net_conn = NULL;
            }
        });
    }
}

#pragma mark - Location shim

typedef void (*LocationCallback)(double lat, double lon);

@interface GS26LocationShim : NSObject <CLLocationManagerDelegate>
@property(nonatomic, strong) CLLocationManager *mgr;
@property(nonatomic, assign) LocationCallback cb;
@end

@implementation GS26LocationShim

- (instancetype)initWithCallback:(LocationCallback)cb {
    self = [super init];
    if (!self) return nil;

    _cb = cb;

    _mgr = [[CLLocationManager alloc] init];
    _mgr.delegate = self;

    // Tune as needed; Best is fine for development.
    _mgr.desiredAccuracy = kCLLocationAccuracyBest;

    // Request permission
    if ([_mgr respondsToSelector:@selector(requestWhenInUseAuthorization)]) {
        [_mgr requestWhenInUseAuthorization];
    }

    // Start updates (will begin delivering after permission is granted)
    [_mgr startUpdatingLocation];

    return self;
}

- (void)locationManager:(CLLocationManager *)manager
     didUpdateLocations:(NSArray<CLLocation *> *)locations
{
    CLLocation *last = locations.lastObject;
    if (!last || !self.cb) return;

    self.cb(last.coordinate.latitude, last.coordinate.longitude);
}

- (void)locationManager:(CLLocationManager *)manager
       didFailWithError:(NSError *)error
{
    // Optional: log or handle errors. Keep silent by default.
    (void)manager;
    (void)error;
}

@end

// Keep the shim alive for the lifetime of the app (simple C interface for Rust).
static GS26LocationShim *g_shim = nil;

// Export a plain C symbol Rust can link against.
void gs26_location_start(LocationCallback cb) {
    @autoreleasepool {
        g_shim = [[GS26LocationShim alloc] initWithCallback:cb];
    }
}
