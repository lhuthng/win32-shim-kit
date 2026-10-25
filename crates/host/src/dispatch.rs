//! The Win32 boundary: one slot per IAT entry, plus the stack rule the
//! interpreter needs to call a host function.
//!
//! The table itself belongs to the target, not to this crate: it is generated
//! from a specific executable (`Slot` list supplied as [`Table`]), because the
//! IAT addresses and the call-site arities are properties of one binary. This
//! module only supplies the type and the lookup, so a port can carry its own
//! table and the engine stays target-free.
//!
//! Arity is *not* needed to return correctly. Every one of these imports is
//! stdcall, so the callee pops its own arguments and the guest's stack pointer
//! returns to the value it had before the `call` pushed the return address. The
//! interpreter saves that value per call site, which is also why most slots may
//! legitimately have no agreed arity. The arity is used for reading arguments in
//! a named form and for reporting contradictions.

/// One imported function and the stack contract derived from its call sites.
#[derive(Clone, Copy, Debug)]
pub struct Slot {
    pub dll: &'static str,
    pub name: &'static str,
    pub iat_va: u32,
    /// Bytes of arguments on the stack at the call, or `None` when the sites
    /// disagreed and a human has to resolve it.
    pub args: Option<u16>,
    /// Call sites found, how many agreed with `args`, and how many could not
    /// vote because their pushes may have been register saves.
    pub sites: u16,
    pub agreement: u16,
    pub ambiguous: u16,
}

/// The import surface of one executable.
///
/// Sorted by `iat_va`, so a lookup is a binary search. The order is a
/// requirement, not a convenience: [`Table::slot`] relies on it.
#[derive(Clone, Copy, Debug)]
pub struct Table {
    pub slots: &'static [Slot],
}

impl Default for Table {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl Table {
    /// A table with no imports, for a machine that is not booting an image yet.
    pub const EMPTY: Table = Table { slots: &[] };

    /// Looks up a slot by the address the guest calls through.
    pub fn slot(&self, iat_va: u32) -> Option<&'static Slot> {
        let index = self.index_of(iat_va)?;
        Some(&self.slots[index])
    }

    /// Returns the index the guest's IAT slot must hold for a host function.
    pub fn index_of(&self, iat_va: u32) -> Option<usize> {
        self.slots.binary_search_by_key(&iat_va, |slot| slot.iat_va).ok()
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'static, Slot> {
        self.slots.iter()
    }
}

/// Stack pointer after a stdcall import returns and the guest is about to
/// continue its caller. `before_call` is the ESP the interpreter recorded just
/// before it pushed the return address.
pub fn stdcall_return_esp(before_call: u32) -> u32 {
    before_call
}

/// Stack pointer after a cdecl import returns: the caller cleans the arguments.
/// The analyzed build imports no cdecl functions, so this exists for the CRT
/// helpers a future build might add.
pub fn cdecl_return_esp(before_call: u32, args: u16) -> u32 {
    before_call.wrapping_sub(u32::from(args))
}

impl Slot {
    /// Argument bytes, when the call sites agree.
    pub fn arg_bytes(&self) -> Option<u32> {
        self.args.map(u32::from)
    }

    /// Human-readable slot name for diagnostics: `KERNEL32.dll!ReadFile`.
    pub fn label(&self) -> String {
        format!("{}!{}", self.dll, self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table shaped like a generated one: ascending IAT addresses, one slot
    /// with an agreed arity and one without.
    static SLOTS: [Slot; 3] = [
        Slot { dll: "KERNEL32.dll", name: "GetVersion", iat_va: 0x0040_1000, args: None, sites: 1, agreement: 0, ambiguous: 0 },
        Slot { dll: "KERNEL32.dll", name: "ReadFile", iat_va: 0x0040_1004, args: Some(20), sites: 2, agreement: 2, ambiguous: 0 },
        Slot { dll: "USER32.dll", name: "MessageBoxA", iat_va: 0x0040_1008, args: Some(16), sites: 1, agreement: 1, ambiguous: 0 },
    ];

    static TABLE: Table = Table { slots: &SLOTS };

    #[test]
    fn finds_slots_by_address_and_rejects_unknown_ones() {
        let read_file = &SLOTS[1];
        assert_eq!(TABLE.slot(read_file.iat_va).map(|found| found.name), Some("ReadFile"));
        assert_eq!(TABLE.index_of(read_file.iat_va), Some(1));
        assert_eq!(read_file.label(), "KERNEL32.dll!ReadFile");
        assert_eq!(read_file.arg_bytes(), Some(20));
        assert_eq!(SLOTS[0].arg_bytes(), None);

        assert!(TABLE.slot(0x0040_0000).is_none());
        assert!(TABLE.index_of(0).is_none());
        assert_eq!(TABLE.len(), 3);
        assert!(Table::EMPTY.is_empty());
    }

    #[test]
    fn stdcall_returns_to_the_pre_call_stack_pointer() {
        assert_eq!(stdcall_return_esp(0x00ff_ff00), 0x00ff_ff00);
        assert_eq!(cdecl_return_esp(0x00ff_ff00, 12), 0x00ff_fef4);
        assert_eq!(cdecl_return_esp(4, 8), 0xffff_fffc);
    }
}
