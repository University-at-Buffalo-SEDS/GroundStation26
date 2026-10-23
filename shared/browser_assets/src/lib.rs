//! Static browser assets are owned by the selected frontend checkout.
#![no_std]
include!(concat!(env!("OUT_DIR"), "/browser_assets.rs"));
