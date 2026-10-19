//! Runs the unit tests in `build/sha.rs`, the helper `build.rs` includes.
//! Cargo never runs tests inside a build script, so they run from here.
#![allow(dead_code)]

mod sha {
    include!("../build/sha.rs");
}
