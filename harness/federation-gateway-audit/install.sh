# Builds the router from the checkout in PROTOTYPE_ROUTER_DIR and copies the
# binary next to this script.
set -e
: "${PROTOTYPE_ROUTER_DIR:?set PROTOTYPE_ROUTER_DIR to the prototype-router checkout}"
cargo build --release --manifest-path "$PROTOTYPE_ROUTER_DIR/Cargo.toml"
cp "$PROTOTYPE_ROUTER_DIR/target/release/prototype-router" ./prototype-router
