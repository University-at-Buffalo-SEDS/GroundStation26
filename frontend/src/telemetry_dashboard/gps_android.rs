#![allow(dead_code)]
#![cfg(target_os = "android")]

use ::jni::objects::{JClass, JObject, JString, JValue};
use ::jni::signature::RuntimeMethodSignature;
use ::jni::strings::JNIString;
use ::jni::sys::{jdouble, jfloat};
use ::jni::{Env, EnvUnowned, JavaVM};
use ndk_context::android_context;
use std::sync::atomic::{AtomicU64, Ordering};

static LAT_BITS: AtomicU64 = AtomicU64::new(f64::NAN.to_bits());
static LON_BITS: AtomicU64 = AtomicU64::new(f64::NAN.to_bits());
static HEADING_BITS: AtomicU64 = AtomicU64::new(f64::NAN.to_bits());

const LOCATION_SHIM_CLASS_DOT: &str = "com.ubseds.gs26.LocationShim";

fn with_android_env<R>(f: impl FnOnce(&mut Env<'_>, &JObject<'_>) -> R) -> Option<R> {
    let ctx = android_context();
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) };
    vm.attach_current_thread(|env| -> ::jni::errors::Result<R> {
        let context = unsafe { JObject::from_raw(env, ctx.context().cast()) };
        let result = f(env, &context);
        let _ = context.into_raw();
        Ok(result)
    })
    .ok()
}

fn call_static_void(method: &str, sig: &str, args: &[JValue<'_>]) {
    let _ = with_android_env(|env, context| {
        let method_name = method;
        let method = JNIString::from(method_name);
        let parsed_sig = match RuntimeMethodSignature::from_str(sig) {
            Ok(sig) => sig,
            Err(err) => {
                eprintln!("Android bridge signature parse failed: {err}");
                return;
            }
        };
        let class_loader = match env
            .call_method(
                context,
                ::jni::jni_str!("getClassLoader"),
                ::jni::jni_sig!("()Ljava/lang/ClassLoader;"),
                &[],
            )
            .and_then(|value| value.l())
        {
            Ok(loader) => loader,
            Err(err) => {
                eprintln!("Android class loader lookup failed: {err}");
                return;
            }
        };
        let class_name: JString<'_> = match env.new_string(LOCATION_SHIM_CLASS_DOT) {
            Ok(name) => name,
            Err(err) => {
                eprintln!("Android bridge class name creation failed: {err}");
                return;
            }
        };
        let class = match env
            .call_method(
                &class_loader,
                ::jni::jni_str!("loadClass"),
                ::jni::jni_sig!("(Ljava/lang/String;)Ljava/lang/Class;"),
                &[JValue::Object(&JObject::from(class_name))],
            )
            .and_then(|value| value.l())
        {
            Ok(class) => class,
            Err(err) => {
                eprintln!("Android bridge class lookup failed: {err}");
                return;
            }
        };
        let class = unsafe { JClass::from_raw(env, class.into_raw().cast()) };
        let mut owned_args = Vec::with_capacity(args.len() + 1);
        if sig.starts_with("(Landroid/content/Context;") {
            owned_args.push(JValue::Object(context));
        }
        owned_args.extend_from_slice(args);
        if let Err(err) =
            env.call_static_method(&class, &method, &parsed_sig.method_signature(), &owned_args)
        {
            eprintln!("Android bridge call {method_name} failed: {err}");
        }
    });
}

pub fn start() {
    call_static_void("start", "(Landroid/content/Context;)V", &[]);
}

pub fn stop() {
    call_static_void("stop", "()V", &[]);
}

pub fn set_keep_screen_on(enabled: bool) {
    call_static_void(
        "setKeepScreenOn",
        "(Landroid/content/Context;Z)V",
        &[JValue::Bool(enabled)],
    );
}

pub fn latest_heading_deg() -> Option<f64> {
    let v = f64::from_bits(HEADING_BITS.load(Ordering::Relaxed));
    if v.is_finite() { Some(v) } else { None }
}

pub fn latest_location() -> Option<(f64, f64)> {
    let lat = f64::from_bits(LAT_BITS.load(Ordering::Relaxed));
    let lon = f64::from_bits(LON_BITS.load(Ordering::Relaxed));
    if lat.is_finite() && lon.is_finite() {
        Some((lat, lon))
    } else {
        None
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubseds_gs26_LocationShim_nativeOnLocationUpdate(
    _env: EnvUnowned<'_>,
    _class: JClass<'_>,
    lat: jdouble,
    lon: jdouble,
) {
    if lat.is_finite() && lon.is_finite() {
        LAT_BITS.store(lat.to_bits(), Ordering::Relaxed);
        LON_BITS.store(lon.to_bits(), Ordering::Relaxed);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubseds_gs26_LocationShim_nativeOnHeadingUpdate(
    _env: EnvUnowned<'_>,
    _class: JClass<'_>,
    heading_deg: jfloat,
) {
    let deg = f64::from(heading_deg);
    if deg.is_finite() {
        HEADING_BITS.store(deg.to_bits(), Ordering::Relaxed);
    }
}
