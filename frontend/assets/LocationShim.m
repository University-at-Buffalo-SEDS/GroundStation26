#import <Foundation/Foundation.h>
#import <CoreLocation/CoreLocation.h>

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
    _mgr.desiredAccuracy = kCLLocationAccuracyBest;

    if ([_mgr respondsToSelector:@selector(requestWhenInUseAuthorization)]) {
        [_mgr requestWhenInUseAuthorization];
    }

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

@end

static GS26LocationShim *g_shim = nil;

// Export a plain C symbol Rust can link against.
void gs26_location_start(LocationCallback cb) {
    @autoreleasepool {
        g_shim = [[GS26LocationShim alloc] initWithCallback:cb];
    }
}
