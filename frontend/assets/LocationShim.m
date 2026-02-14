// LocationShim.m
//
// iOS/macOS Objective-C shim for:
//  - Requesting Location permission + streaming coordinates to a Rust callback
//  - Triggering the Local Network permission prompt by doing a short-lived TCP
//  connect
//    using Network.framework to the user-provided host/port
//  - Preventing sleep on iOS via UIApplication.idleTimerDisabled (no-op on
//  macOS)
//
// Build:
//  - Compile with ARC: -fobjc-arc
//  - Link frameworks: CoreLocation.framework, Network.framework
//  - iOS only: UIKit.framework
//

#import <CoreLocation/CoreLocation.h>
#import <Foundation/Foundation.h>
#import <Network/Network.h>
#import <TargetConditionals.h>
#import <dispatch/dispatch.h>

#if TARGET_OS_IOS
#import <UIKit/UIKit.h>
#endif

#pragma mark - Location shim

typedef void (*LocationCallback)(double lat, double lon);

@interface GS26LocationShim : NSObject <CLLocationManagerDelegate>
@property(nonatomic, strong) CLLocationManager *mgr;
@property(nonatomic, assign) LocationCallback cb;
@end

@implementation GS26LocationShim

- (instancetype)initWithCallback:(LocationCallback)cb {
  self = [super init];
  if (!self)
    return nil;

  _cb = cb;

  _mgr = [[CLLocationManager alloc] init];
  _mgr.delegate = self;
  _mgr.desiredAccuracy = kCLLocationAccuracyBest;

  CLAuthorizationStatus status;
  if ([_mgr respondsToSelector:@selector(authorizationStatus)]) {
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wunguarded-availability-new"
    status = _mgr.authorizationStatus;
#pragma clang diagnostic pop
  } else {
    status = [CLLocationManager authorizationStatus];
  }

  if (status == kCLAuthorizationStatusNotDetermined &&
      [_mgr respondsToSelector:@selector(requestWhenInUseAuthorization)]) {
    [_mgr requestWhenInUseAuthorization];
  }

  [_mgr startUpdatingLocation];
  return self;
}

- (void)locationManager:(CLLocationManager *)manager
     didUpdateLocations:(NSArray<CLLocation *> *)locations {
  (void)manager;
  CLLocation *last = locations.lastObject;
  if (!last || !self.cb)
    return;
  self.cb(last.coordinate.latitude, last.coordinate.longitude);
}

- (void)locationManager:(CLLocationManager *)manager
       didFailWithError:(NSError *)error {
  (void)manager;
  (void)error;
}

@end

static GS26LocationShim *g_shim = nil;

// Export a plain C symbol Rust can link against.
void gs26_location_start(LocationCallback cb) {
  @autoreleasepool {
    g_shim = [[GS26LocationShim alloc] initWithCallback:cb];
  }
}

#pragma mark - Keep awake (iOS only)

// iOS: prevent sleep while app is open.
// macOS: no-op.
void gs26_set_idle_timer_disabled(int disabled) {
#if TARGET_OS_IOS
  dispatch_async(dispatch_get_main_queue(), ^{
    [UIApplication sharedApplication].idleTimerDisabled = (disabled != 0);
  });
#else
  (void)disabled;
#endif
}

#pragma mark - Local Network prompt shim (Network.framework)

static dispatch_queue_t gs26_net_queue(void) {
  static dispatch_queue_t q;
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    q = dispatch_queue_create("com.ubsed.gs26.localnet", DISPATCH_QUEUE_SERIAL);
  });
  return q;
}

// Keep short-lived connections alive until their callbacks fire.
static NSMutableSet<id> *gs26_live_conns(void) {
  static NSMutableSet<id> *s;
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    s = [[NSMutableSet alloc] init];
  });
  return s;
}

static NSString *gs26_string_from_c(const char *s) {
  if (!s)
    return nil;
  return [NSString stringWithUTF8String:s];
}

// Make a short-lived TCP connection attempt.
// - Resolves hostname (Network.framework)
// - Attempts connect
// - Cancels on ready/failed/cancelled or after timeout
static void gs26_poke_host_port(NSString *host, uint16_t port,
                                uint32_t timeout_ms) {
  if (host.length == 0)
    return;
  if (port == 0)
    port = 80;
  if (timeout_ms == 0)
    timeout_ms = 900;

  dispatch_queue_t q = gs26_net_queue();

  NSString *portStr = [NSString stringWithFormat:@"%u", (unsigned)port];
  nw_endpoint_t ep =
      nw_endpoint_create_host(host.UTF8String, portStr.UTF8String);

  // Plain TCP (TLS disabled).
  // Modern Network.framework supports:
  //   nw_parameters_create_secure_tcp(NW_PARAMETERS_DISABLE_PROTOCOL,
  //                                  NW_PARAMETERS_DEFAULT_CONFIGURATION);
  nw_parameters_t params = nw_parameters_create_secure_tcp(
      NW_PARAMETERS_DISABLE_PROTOCOL, NW_PARAMETERS_DEFAULT_CONFIGURATION);

  nw_connection_t conn = nw_connection_create(ep, params);
  nw_connection_set_queue(conn, q);

  @synchronized(gs26_live_conns()) {
    [gs26_live_conns() addObject:conn];
  }

  __block BOOL finished = NO;

  void (^finish)(void) = ^{
    if (finished)
      return;
    finished = YES;

    nw_connection_cancel(conn);

    @synchronized(gs26_live_conns()) {
      [gs26_live_conns() removeObject:conn];
    }
  };

  nw_connection_set_state_changed_handler(
      conn, ^(nw_connection_state_t state, nw_error_t error) {
        (void)error;

        if (state == nw_connection_state_ready) {
          finish();
          return;
        }

        if (state == nw_connection_state_failed ||
            state == nw_connection_state_cancelled) {
          finish();
          return;
        }
      });

  nw_connection_start(conn);

  dispatch_after(dispatch_time(DISPATCH_TIME_NOW,
                               (int64_t)timeout_ms * (int64_t)NSEC_PER_MSEC),
                 q, ^{
                   finish();
                 });
}

// Public C API: poke a host + port
void gs26_localnet_poke_host_port(const char *host, uint16_t port) {
  @autoreleasepool {
    NSString *h = gs26_string_from_c(host);
    gs26_poke_host_port(h, port, 900);
  }
}

// Public C API: poke from a URL like "http://192.168.1.50:3000" or
// "https://example.com:3000"
void gs26_localnet_poke_url(const char *url_cstr) {
  @autoreleasepool {
    NSString *urlStr = gs26_string_from_c(url_cstr);
    if (urlStr.length == 0)
      return;

    NSURLComponents *c = [NSURLComponents componentsWithString:urlStr];
    NSString *host = c.host;
    NSNumber *portNum = c.port;

    // If user typed without scheme, try adding http://
    if (host.length == 0) {
      NSURLComponents *c2 = [NSURLComponents
          componentsWithString:[@"http://" stringByAppendingString:urlStr]];
      host = c2.host;
      portNum = c2.port;
      c = c2;
    }

    if (host.length == 0)
      return;

    uint16_t port = 80;
    if (portNum != nil) {
      NSInteger p = portNum.integerValue;
      if (p > 0 && p <= 65535)
        port = (uint16_t)p;
    } else {
      NSString *scheme = c.scheme.lowercaseString;
      if ([scheme isEqualToString:@"https"])
        port = 443;
    }

    gs26_poke_host_port(host, port, 900);
  }
}
