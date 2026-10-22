//! Print the OpenAPI 3.1 document generated from the server.
//!
//! ```sh
//! cargo run --quiet --example openapi --features serve
//! ```

fn main() {
    print!("{}", neutronsync::server::openapi_json());
}
