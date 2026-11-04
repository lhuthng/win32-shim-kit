//! Menus (`USER32.dll`): the bar a game hangs off its window, and the item state
//! it reads back.
//!
//! A menu is *real* state here rather than a constant, because a game from this
//! era decides what to offer from what it can read back: `Load` is greyed until a
//! save exists, `Continue` is checked while one is loaded, and the code that
//! paints the bar asks `GetMenuItemCount` and `GetMenuState` for every line. A
//! host that answered zero to all of it would boot and then refuse to start a
//! game.
//!
//! `LoadMenuA` reads the standard `MENU` template out of the image's resource
//! tree -- `RT_MENU` is type 4 -- so the item ids, the text and the greyed state
//! are the ones the linker wrote. The template's data forms are the ones a
//! resource compiler emits: `0x00`/`0x01` are followed by the item's text,
//! `0x02`/`0x03` carry none (a separator), `0x10` marks a popup whose children
//! follow the popup's own title, and `0x20` marks the item greyed. A form this
//! host does not know ends the run rather than guessing at a length, which is the
//! one failure that could read a menu out of unrelated bytes.
//!
//! What the bar *looks* like is not modelled: the page is the window, the guest
//! draws its own screens, and this host has no system menu to draw. What a guest
//! can observe is the item state it asked for, which is what these calls answer.

use crate::memory::GuestMemory;
use crate::pe::{ResourceKey, IMAGE_BASE};
use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use super::{Menu, MenuItem};

use Action::Return;

const USER32: &str = "USER32.dll";

/// `RT_MENU`: the resource type a menu template is filed under.
const RT_MENU: u32 = 4;

/// `MF_*`: the item state bits this host keeps.
const MF_BYPOSITION: u32 = 0x0000_0400;
const MF_SEPARATOR: u32 = 0x0000_0800;
const MF_ENABLED: u32 = 0;
const MF_GRAYED: u32 = 0x0000_0001;
const MF_DISABLED: u32 = 0x0000_0002;
const MF_CHECKED: u32 = 0x0000_0008;
const MF_UNCHECKED: u32 = 0;
const MF_POPUP: u32 = 0x0000_0010;

/// The state bits `GetMenuState` reports, and the ones `EnableMenuItem` and
/// `CheckMenuItem` move.
const STATE_BITS: u32 = MF_GRAYED | MF_DISABLED | MF_CHECKED;

/// What a lookup that found no item answers, which is what Windows' `0xffff_ffff`
/// means to a guest that compares the answer with `MF_ENABLED`.
const NO_ITEM: u32 = 0xffff_ffff;

/// A `MENUHEADER` is two words: the template version and the size of the header
/// itself, which is added to the four bytes already read.
const MENU_HEADER: u32 = 4;

/// `MAKELONG(13, 13)`: the size of a check mark in dialog units, which is what a
/// guest uses to indent its items when it draws the bar itself.
const CHECK_MARK: u32 = 0x000d_000d;

pub const SHIMS: &[Binding] = &[
    Binding { dll: USER32, name: "LoadMenuA", arg_bytes: 8, shim: load_menu },
    Binding { dll: USER32, name: "DestroyMenu", arg_bytes: 4, shim: destroy_menu },
    Binding { dll: USER32, name: "DeleteMenu", arg_bytes: 12, shim: delete_menu },
    Binding { dll: USER32, name: "DrawMenuBar", arg_bytes: 4, shim: draw_menu_bar },
    Binding { dll: USER32, name: "SetMenu", arg_bytes: 8, shim: set_menu },
    Binding { dll: USER32, name: "GetSubMenu", arg_bytes: 8, shim: get_sub_menu },
    Binding { dll: USER32, name: "GetMenuItemCount", arg_bytes: 4, shim: get_menu_item_count },
    Binding { dll: USER32, name: "GetMenuItemID", arg_bytes: 8, shim: get_menu_item_id },
    Binding { dll: USER32, name: "GetMenuState", arg_bytes: 12, shim: get_menu_state },
    Binding { dll: USER32, name: "CheckMenuItem", arg_bytes: 12, shim: check_menu_item },
    Binding { dll: USER32, name: "EnableMenuItem", arg_bytes: 12, shim: enable_menu_item },
    Binding { dll: USER32, name: "ModifyMenuA", arg_bytes: 20, shim: modify_menu },
    Binding { dll: USER32, name: "SetMenuItemBitmaps", arg_bytes: 20, shim: set_menu_item_bitmaps },
    Binding { dll: USER32, name: "GetMenuCheckMarkDimensions", arg_bytes: 0, shim: check_mark_dimensions },
];

/// `LoadMenuA(hInstance, name)`: the menu template the image carries, as a handle
/// whose items a guest can query.
///
/// The instance is ignored -- this host has one image, and its resources are the
/// only ones a menu could come from. A name the resource tree does not hold
/// answers an *empty* menu rather than zero: a guest that calls `SetMenu` with
/// the zero `LoadMenuA` failed with falls back to drawing nothing, and the notice
/// says which id it asked for, which is what a port needs to know.
fn load_menu(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let name = call.arg(1)?;
    let key = if name < 0x1_0000 {
        ResourceKey::Id(name)
    } else {
        ResourceKey::Name(call.memory.read_cstr(name)?)
    };
    // Where the template is, copied out so no borrow of the resource table is
    // held while the parser registers the submenus it finds.
    let found = host
        .resources
        .iter()
        .find(|resource| resource.kind == ResourceKey::Id(RT_MENU) && resource.name == key)
        .map(|resource| (resource.rva, resource.size));
    let Some((rva, size)) = found else {
        let id = match &key {
            ResourceKey::Id(id) => format!("{id}"),
            ResourceKey::Name(text) => format!("{text:?}"),
        };
        host.notice(format!(
            "LoadMenuA: the image carries no MENU resource {id}, so the guest gets an empty menu"
        ));
        let handle = host.windows.add_menu(Menu::default());
        return Ok(Return(handle));
    };
    let at = IMAGE_BASE.wrapping_add(rva);
    if size < MENU_HEADER {
        return Err(format!(
            "LoadMenuA: the MENU resource at {rva:#010x} is {size} bytes, shorter than a MENUHEADER"
        ));
    }
    // The header's two words are the version and the header's own size; the items
    // start after both.
    let header = call.memory.read_u16(at + 2)?;
    let items_at = at + MENU_HEADER + u32::from(header);
    let (items, _) = parse_items(host, call.memory, items_at)?;
    if items.is_empty() {
        host.notice(format!(
            "LoadMenuA: the MENU resource at {rva:#010x} parsed to no items, which its data forms did not match"
        ));
    }
    let resource = match key {
        ResourceKey::Id(id) => id,
        ResourceKey::Name(_) => 0,
    };
    let handle = host.windows.add_menu(Menu { items, resource });
    Ok(Return(handle))
}

/// Parses one run of menu items, answering the items and where the run ended.
///
/// A run ends at the zero word a resource compiler writes after the last item. A
/// popup's children are a run of their own, so this recurses -- on the flat
/// template, which is why it answers where it stopped.
fn parse_items(
    host: &mut Host,
    memory: &GuestMemory,
    mut at: u32,
) -> Result<(Vec<MenuItem>, u32)> {
    let mut items = Vec::new();
    loop {
        let resinfo = memory.read_u16(at)?;
        at += 2;
        if resinfo == 0 {
            return Ok((items, at));
        }
        // The id is always filed in the same place, whatever the data form: a
        // resource compiler emits it even for a popup, where the command id is
        // the popup's own position rather than anything a guest reads.
        let id = u32::from(memory.read_u16(at)?);
        at += 2;
        let form = u32::from(resinfo) & 0x000f;
        let text = if form <= 1 {
            let (text, next) = read_string(memory, at)?;
            at = next;
            text
        } else if form <= 3 {
            String::new()
        } else {
            // A form this parser does not know: stop before reading whatever it
            // holds, which is the difference between an empty menu and a menu
            // built out of unrelated bytes.
            return Ok((items, at));
        };
        let mut flags = 0;
        if u32::from(resinfo) & 0x0020 != 0 {
            flags |= MF_GRAYED;
        }
        let submenu = if u32::from(resinfo) & MF_POPUP != 0 {
            let (children, next) = parse_items(host, memory, at)?;
            at = next;
            let child = host.windows.add_menu(Menu {
                items: children,
                resource: 0,
            });
            flags |= MF_POPUP;
            Some(child)
        } else {
            None
        };
        if form >= 2 && submenu.is_none() {
            // A form with no text is a separator.
            flags |= MF_SEPARATOR;
        }
        items.push(MenuItem {
            id: if submenu.is_some() { 0 } else { id },
            flags,
            text,
            submenu,
        });
    }
}

/// Reads a NUL-terminated string, answering it and where it ended.
fn read_string(memory: &GuestMemory, at: u32) -> Result<(String, u32)> {
    let mut bytes = Vec::new();
    let mut cursor = at;
    // A template's strings are short; the cap is a guard against a missing
    // terminator walking the whole image.
    for _ in 0..4096 {
        let byte = memory.read_u8(cursor)?;
        cursor += 1;
        if byte == 0 {
            break;
        }
        bytes.push(byte);
    }
    Ok((String::from_utf8_lossy(&bytes).into_owned(), cursor))
}

/// The index of the item a `uId`/`uFlags` pair names: by command id, or by
/// position when the guest says so.
fn item_index(menu: &Menu, id: u32, flags: u32) -> Option<usize> {
    if flags & MF_BYPOSITION != 0 {
        let index = id as usize;
        return (index < menu.items.len()).then_some(index);
    }
    menu.items.iter().position(|item| item.id == id)
}

/// `DestroyMenu(hmenu)`: the handle stops naming a menu, and a guest that uses it
/// afterwards gets the zero every query here answers for one it does not know.
fn destroy_menu(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    Ok(Return(u32::from(host.windows.menus.remove(&handle).is_some())))
}

/// `DeleteMenu(hMenu, uPosition, uFlags)`: takes one item off a menu, answering
/// whether it was there.
///
/// The item is named the way every other `MF_BYCOMMAND`/`MF_BYPOSITION` call
/// names one, so it is found by the same [`item_index`]. A popup takes the menu
/// it opens with it, which is what Windows destroys: leaving the submenu in the
/// table would leak a menu no window can reach and no guest can name.
///
/// A menu that was never made answers zero, as a bad handle does on Windows.
/// That is the answer a guest uses to skip the rest of its teardown, so it is a
/// refusal rather than an error.
fn delete_menu(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let id = call.arg(1)?;
    let flags = call.arg(2)?;
    let Some(menu) = host.windows.menu_mut(handle) else {
        return Ok(Return(0));
    };
    let Some(index) = item_index(menu, id, flags) else {
        return Ok(Return(0));
    };
    let submenu = menu.items.remove(index).submenu;
    if let Some(submenu) = submenu {
        host.windows.menus.remove(&submenu);
    }
    Ok(Return(1))
}

/// `DrawMenuBar(hwnd)`: asks for the window's bar to be repainted.
///
/// There is no system bar here -- a guest that wants one drawn asks for its
/// items and paints them itself, which is what the rest of this module is for --
/// so the request is answered by having nothing to do. It answers TRUE, the way
/// Windows does for a window that has a menu, because a guest that checks is
/// checking whether its bar is being maintained, and here it is.
fn draw_menu_bar(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let _ = call.arg(0)?;
    Ok(Return(1))
}

/// `SetMenu(hwnd, hmenu)`: hangs a bar on a window, answering 1 and remembering
/// what was there, which is what `GetMenu` reads back.
fn set_menu(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let menu = call.arg(1)?;
    if !host.windows.exists(window) {
        return Ok(Return(0));
    }
    host.windows.set_menu(window, menu);
    Ok(Return(1))
}

/// `GetSubMenu(hmenu, nPos)`: the popup an item opens, or zero for an item that
/// opens none.
fn get_sub_menu(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let position = call.arg(1)? as usize;
    let Some(menu) = host.windows.menu(handle) else {
        return Ok(Return(0));
    };
    Ok(Return(
        menu.items
            .get(position)
            .and_then(|item| item.submenu)
            .unwrap_or(0),
    ))
}

/// `GetMenuItemCount(hmenu)`: how many items the bar holds, which is how a guest
/// that draws the bar itself knows where to stop.
fn get_menu_item_count(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    Ok(Return(
        host.windows
            .menu(handle)
            .map_or(NO_ITEM, |menu| menu.items.len() as u32),
    ))
}

/// `GetMenuItemID(hmenu, nPos)`: the command id at a position.
///
/// A popup answers `0xffff_ffff`, because the command a client receives for one
/// is the popup's own id and not a number this call reports; a separator answers
/// zero, which is the id `WM_COMMAND` would carry for it.
fn get_menu_item_id(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let position = call.arg(1)? as usize;
    let Some(menu) = host.windows.menu(handle) else {
        return Ok(Return(NO_ITEM));
    };
    let Some(item) = menu.items.get(position) else {
        return Ok(Return(NO_ITEM));
    };
    if item.submenu.is_some() {
        return Ok(Return(NO_ITEM));
    }
    Ok(Return(item.id))
}

/// `GetMenuState(hmenu, uId, uFlags)`: the state bits of one item.
///
/// A guest that greys `Load` out calls this to remember what the state was, so an
/// item that is not there answers `0xffff_ffff` rather than a state it never had.
fn get_menu_state(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let id = call.arg(1)?;
    let flags = call.arg(2)?;
    let Some(menu) = host.windows.menu(handle) else {
        return Ok(Return(NO_ITEM));
    };
    let Some(index) = item_index(menu, id, flags) else {
        return Ok(Return(NO_ITEM));
    };
    let state = menu.items[index].flags & STATE_BITS;
    // An enabled item carries no state bit at all, which is what `MF_ENABLED`
    // being zero means.
    Ok(Return(if state == 0 { MF_ENABLED } else { state }))
}

/// `CheckMenuItem(hmenu, uId, uCheck)`: sets or clears the check mark, answering
/// the previous state so a guest can tell whether it changed.
fn check_menu_item(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let id = call.arg(1)?;
    let check = call.arg(2)?;
    let Some(menu) = host.windows.menu_mut(handle) else {
        return Ok(Return(NO_ITEM));
    };
    let Some(index) = item_index(menu, id, check) else {
        return Ok(Return(NO_ITEM));
    };
    let item = &mut menu.items[index];
    let previous = item.flags & MF_CHECKED;
    if check & MF_CHECKED != 0 {
        item.flags |= MF_CHECKED;
    } else {
        item.flags &= !MF_CHECKED;
    }
    Ok(Return(if previous != 0 { MF_CHECKED } else { MF_UNCHECKED }))
}

/// `EnableMenuItem(hmenu, uId, uEnable)`: enables, greys or disables one item,
/// answering its previous state.
///
/// This is the call a game makes when the state of the world changes -- no save
/// to continue from, or a level it has not reached -- so the answer is the state
/// the item had before, not a success code.
fn enable_menu_item(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let id = call.arg(1)?;
    let enable = call.arg(2)?;
    let Some(menu) = host.windows.menu_mut(handle) else {
        return Ok(Return(NO_ITEM));
    };
    let Some(index) = item_index(menu, id, enable) else {
        return Ok(Return(NO_ITEM));
    };
    let item = &mut menu.items[index];
    let previous = item.flags & (MF_GRAYED | MF_DISABLED);
    item.flags &= !(MF_GRAYED | MF_DISABLED);
    // `MF_ENABLED` is zero, so its own state is the absence of both bits.
    item.flags |= enable & (MF_GRAYED | MF_DISABLED);
    Ok(Return(if previous == 0 { MF_ENABLED } else { previous }))
}

/// `ModifyMenuA(hmenu, uPosition, uFlags, uIDNewItem, lpNewItem)`: rewrites one
/// item -- its id, its text, its state, or the submenu it opens.
///
/// A guest that renames a menu entry after loading a save does this rather than
/// building a new bar, so the item keeps its position.
fn modify_menu(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let position = call.arg(1)?;
    let flags = call.arg(2)?;
    let new_item = call.arg(3)?;
    let text = call.arg(4)?;
    let text = if flags & MF_POPUP != 0 || text == 0 {
        String::new()
    } else {
        call.memory.read_cstr(text)?
    };
    let Some(menu) = host.windows.menu_mut(handle) else {
        return Ok(Return(0));
    };
    let Some(index) = item_index(menu, position, flags) else {
        return Ok(Return(0));
    };
    let item = &mut menu.items[index];
    // The state bits the caller named replace the ones that were there; the rest
    // of the flags -- separator, popup -- are the item's own shape and stay.
    let state = flags & STATE_BITS;
    item.flags = (item.flags & !STATE_BITS) | state;
    if flags & MF_POPUP != 0 {
        item.submenu = Some(new_item);
        item.id = 0;
    } else {
        item.id = new_item;
        item.text = text;
    }
    Ok(Return(1))
}

/// `SetMenuItemBitmaps`: the check marks a guest supplies. This host draws no
/// menu, so the answer is the success Windows reports when the call is accepted.
fn set_menu_item_bitmaps(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

/// `GetMenuCheckMarkDimensions`: the size of a check mark, which a guest that
/// paints its own bar indents its items by.
fn check_mark_dimensions(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(CHECK_MARK))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::STACK_BASE;

    fn machine() -> Option<(Host, GuestMemory)> {
        let image = crate::reference_image()?;
        Some((Host::new(), GuestMemory::load(image).expect("image maps")))
    }

    fn call_shim(
        host: &mut Host,
        memory: &mut GuestMemory,
        name: &str,
        arguments: &[u32],
    ) -> Result<Action> {
        let binding = SHIMS
            .iter()
            .find(|binding| binding.name == name)
            .unwrap_or_else(|| panic!("{name} is implemented"));
        let args = STACK_BASE + 0x2000;
        let mut bytes = Vec::new();
        for argument in arguments {
            bytes.extend_from_slice(&argument.to_le_bytes());
        }
        memory.write_bytes(args, &bytes).unwrap();
        let mut call = Call {
            memory,
            args,
            arg_bytes: binding.arg_bytes,
        };
        (binding.shim)(host, &mut call)
    }

    /// A menu of three items, the middle one a popup over a submenu of its own.
    fn bar(host: &mut Host) -> (u32, u32) {
        let submenu = host.windows.add_menu(Menu::default());
        let mut menu = Menu::default();
        menu.items.push(MenuItem {
            id: 100,
            flags: 0,
            text: "New".to_string(),
            submenu: None,
        });
        menu.items.push(MenuItem {
            id: 0,
            flags: MF_POPUP,
            text: "Options".to_string(),
            submenu: Some(submenu),
        });
        menu.items.push(MenuItem {
            id: 101,
            flags: 0,
            text: "Exit".to_string(),
            submenu: None,
        });
        (host.windows.add_menu(menu), submenu)
    }

    /// `DeleteMenu` names its item the way every other call does, takes a popup's
    /// submenu with it, and refuses a menu it was never given.
    #[test]
    fn a_deleted_item_leaves_the_bar() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let (menu, submenu) = bar(&mut host);

        // By position: the first item.
        assert_eq!(
            call_shim(&mut host, &mut memory, "DeleteMenu", &[menu, 0, MF_BYPOSITION]).unwrap(),
            Return(1)
        );
        let items = &host.windows.menu(menu).unwrap().items;
        assert_eq!(items.len(), 2, "one item went");
        assert_eq!(items[0].id, 0, "the popup moved up into its place");

        // By command id: the last item.
        assert_eq!(
            call_shim(&mut host, &mut memory, "DeleteMenu", &[menu, 101, 0]).unwrap(),
            Return(1)
        );
        assert_eq!(host.windows.menu(menu).unwrap().items.len(), 1);

        // The popup's submenu goes with it, so nothing is left holding a menu no
        // window can reach.
        assert_eq!(
            call_shim(&mut host, &mut memory, "DeleteMenu", &[menu, 0, MF_BYPOSITION]).unwrap(),
            Return(1)
        );
        assert!(host.windows.menu(submenu).is_none(), "the submenu went too");
        assert!(host.windows.menu(menu).unwrap().items.is_empty());

        // An item that is not there, and a menu that is not there, are both a
        // refusal rather than an error: a guest uses the answer to stop tearing
        // down what it does not own.
        assert_eq!(
            call_shim(&mut host, &mut memory, "DeleteMenu", &[menu, 999, 0]).unwrap(),
            Return(0)
        );
        assert_eq!(
            call_shim(&mut host, &mut memory, "DeleteMenu", &[0xdead, 0, 0]).unwrap(),
            Return(0)
        );
    }

    /// Repainting the bar is nothing to do here, and says so successfully.
    #[test]
    fn a_menu_bar_has_nothing_to_repaint() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        assert_eq!(
            call_shim(&mut host, &mut memory, "DrawMenuBar", &[0x1234]).unwrap(),
            Return(1)
        );
    }
}
