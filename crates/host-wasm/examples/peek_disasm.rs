//! Scratch: decode a window of the guest image, then delete this file.

use host::cpu::decode::decode;
use host::memory::GuestMemory;
use host::pe::Image;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut pc = u32::from_str_radix(args[0].trim_start_matches("0x"), 16).unwrap();
    let stop = u32::from_str_radix(args[1].trim_start_matches("0x"), 16).unwrap();
    let image = Image::from_path(std::path::Path::new(&args[2])).expect("parses");
    let memory = GuestMemory::load(image).expect("maps");
    while pc < stop {
        match decode(&memory, pc) {
            Ok(instruction) => {
                println!("{pc:#010x}  {}", instruction.describe());
                pc += u32::from(instruction.len);
            }
            Err(error) => {
                println!("{pc:#010x}  <{error}>");
                break;
            }
        }
    }
}
