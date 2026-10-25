#!/usr/bin/env bash
# Generate the example shim modules and type-check them against a stub of the
# host API. This is the check that keeps the generator honest: a map that
# validates but generates code the host cannot compile is still broken.
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
out="$here/target/compile-check"
mkdir -p "$out"

for map in kernel32 user32 ddraw; do
    cargo run --quiet --manifest-path "$here/Cargo.toml" -p shimgen -- \
        generate "$here/examples/$map.json" --out "$out/$map.rs"
done

cat > "$out/harness.rs" <<'RUST'
//! The host API surface the generator targets, reduced to what generated code
//! touches. Kept in the check script so the kit has no crate to depend on.
#![allow(dead_code)]

pub type Result<T> = std::result::Result<T, String>;

pub mod shim {
    use crate::Result;

    #[derive(Debug, PartialEq)]
    pub enum Action {
        Return(u32),
        Exit(u32),
    }

    pub struct GuestMemory;
    impl GuestMemory {
        pub fn read_u32(&self, _address: u32) -> Result<u32> {
            Ok(0)
        }
    }

    pub struct Call<'a> {
        pub memory: &'a GuestMemory,
    }
    impl Call<'_> {
        pub fn arg(&self, _index: u32) -> Result<u32> {
            Ok(0)
        }
        pub fn write_u32(&mut self, _address: u32, _value: u32) -> Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    pub struct Input;
    impl Input {
        pub fn async_state(&mut self, _key: u8) -> u16 {
            0
        }
    }

    #[derive(Default)]
    pub struct Windows {
        pub cursor_visible: i32,
    }

    #[derive(Default)]
    pub struct Ddraw;
    impl Ddraw {
        pub fn palette_revision(&self) -> u32 {
            0
        }
    }

    #[derive(Default)]
    pub struct Host {
        pub last_error: u32,
        pub input: Input,
        pub windows: Windows,
        pub ddraw: Ddraw,
    }
    impl Host {
        pub fn allocate(&mut self, _size: u32, _align: u32) -> Result<u32> {
            Ok(0)
        }
        pub fn release(&mut self, _pointer: u32) {}
        pub fn notice(&mut self, _text: String) {}
    }

    pub type Shim = fn(&mut Host, &mut Call<'_>) -> Result<Action>;
    pub struct Binding {
        pub dll: &'static str,
        pub name: &'static str,
        pub arg_bytes: u32,
        pub shim: Shim,
    }
}

mod kernel32 {
    include!("kernel32.rs");
}
mod user32 {
    include!("user32.rs");
}
mod ddraw {
    include!("ddraw.rs");
}

pub fn tables() -> [&'static [shim::Binding]; 3] {
    [kernel32::SHIMS, user32::SHIMS, ddraw::SHIMS]
}
RUST

rustc --edition 2021 --crate-type lib --out-dir "$out" "$out/harness.rs" -A warnings

count=$(( $(grep -c '^    Binding {' "$out"/*.rs | awk -F: '{sum += $2} END {print sum}') ))
echo "type-checked $count generated bindings"
