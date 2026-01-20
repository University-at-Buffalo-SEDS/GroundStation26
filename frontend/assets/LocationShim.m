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
