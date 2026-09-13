//! Local executable icons and names for the native application list.
//!
//! Extraction reads an EXE's icon resources through Shell; it never launches the
//! application. Names come from FileDescription version resources, never window
//! titles. Successful reads and failures are cached on the calling UI thread.

use std::{
    cell::RefCell,
    collections::VecDeque,
    ffi::OsStr,
    os::windows::ffi::OsStrExt,
    path::{Component, Path, PathBuf, Prefix},
    ptr::{NonNull, null_mut},
};

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use windows::{
    Win32::{
        Graphics::Gdi::{
            BI_RGB, BITMAPINFO, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC,
            DeleteObject, GdiFlush, HBITMAP, HDC, HGDIOBJ, SelectObject,
        },
        Storage::FileSystem::{
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_REPARSE_POINT,
            GetDriveTypeW, GetFileAttributesW, GetFileVersionInfoSizeW, GetFileVersionInfoW,
            INVALID_FILE_ATTRIBUTES, VerQueryValueW,
        },
        System::WindowsProgramming::{DRIVE_CDROM, DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOVABLE},
        UI::{
            Shell::SHDefExtractIconW,
            WindowsAndMessaging::{DI_NORMAL, DestroyIcon, DrawIconEx, HICON},
        },
    },
    core::PCWSTR,
};

const ICON_SIZE: u32 = 48;
// At 32 bits per pixel every scanline is already DWORD aligned. The negative
// DIB height below makes its memory order top-to-bottom, matching Slint.
const STRIDE: usize = ICON_SIZE as usize * 4;
const BYTE_COUNT: usize = STRIDE * ICON_SIZE as usize;
const CACHE_CAPACITY: usize = 256;
const MAX_VERSION_RESOURCE_BYTES: u32 = 4 * 1024 * 1024;
const MAX_DESCRIPTION_UTF16_UNITS: usize = 1_024;

thread_local! {
    static CACHE: RefCell<VecDeque<(String, Image)>> = const { RefCell::new(VecDeque::new()) };
    static NAME_CACHE: RefCell<VecDeque<(String, Option<String>)>> = const { RefCell::new(VecDeque::new()) };
}

/// Return the application's actual 48-pixel icon, or an empty image for the
/// caller's generic fallback. Only `path:` identities on local drives qualify.
///
/// The bounded cache includes failures, so periodic timeline refreshes do not
/// repeatedly touch the executable. Icons remain cached for this process's
/// lifetime unless evicted; an updated executable may keep its previous icon.
pub fn for_identity(identity: &str) -> Image {
    if let Some(image) = CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let index = cache.iter().position(|(key, _)| key == identity)?;
        let entry = cache.remove(index)?;
        let image = entry.1.clone();
        cache.push_front(entry);
        Some(image)
    }) {
        return image;
    }

    // Do not hold the RefCell borrow while calling Shell or GDI.
    let image = extract(identity).unwrap_or_default();
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= CACHE_CAPACITY {
            cache.pop_back();
        }
        cache.push_front((identity.to_owned(), image.clone()));
    });
    image
}

/// Prefer a readable FileDescription from the executable's version resource.
/// Unsupported identities or unavailable metadata use the supplied stored name.
/// Failures cache None, rather than the fallback, so later stored-name changes
/// remain visible. The same local-path restrictions and cache bounds as icons
/// apply; an updated EXE's description may remain cached until eviction/restart.
pub fn display_name(identity: &str, fallback: &str) -> String {
    let cached = NAME_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let index = cache.iter().position(|(key, _)| key == identity)?;
        let entry = cache.remove(index)?;
        let name = entry.1.clone();
        cache.push_front(entry);
        Some(name)
    });
    let name = match cached {
        Some(name) => name,
        None => {
            let name = read_file_description(identity);
            NAME_CACHE.with(|cache| {
                let mut cache = cache.borrow_mut();
                if cache.len() >= CACHE_CAPACITY {
                    cache.pop_back();
                }
                cache.push_front((identity.to_owned(), name.clone()));
            });
            name
        }
    };
    name.unwrap_or_else(|| fallback.to_owned())
}

fn read_file_description(identity: &str) -> Option<String> {
    let path = local_executable_path(identity)?;
    let size = unsafe { GetFileVersionInfoSizeW(PCWSTR(path.as_ptr()), None) };
    if size == 0 || size > MAX_VERSION_RESOURCE_BYTES {
        return None;
    }
    let mut resource = vec![0_u8; size as usize];
    unsafe {
        GetFileVersionInfoW(
            PCWSTR(path.as_ptr()),
            None,
            size,
            resource.as_mut_ptr().cast(),
        )
    }
    .ok()?;

    // Translation contains little-endian WORD language/code-page pairs. Use
    // the file's own translation order without guessing an application's brand.
    let mut translations = version_value(&resource, r"\VarFileInfo\Translation", 1)
        .into_iter()
        .flat_map(|bytes| bytes.chunks_exact(4))
        .take(256)
        .map(|pair| {
            (
                u16::from_le_bytes([pair[0], pair[1]]),
                u16::from_le_bytes([pair[2], pair[3]]),
            )
        })
        .collect::<Vec<_>>();
    // Some files omit Translation while retaining the conventional Unicode or
    // Windows-1252 string table. These are resource keys, not name mappings.
    for translation in [(0x0409, 0x04b0), (0x0409, 0x04e4), (0x0000, 0x04b0)] {
        if !translations.contains(&translation) {
            translations.push(translation);
        }
    }
    for (language, code_page) in translations {
        let query = format!(r"\StringFileInfo\{language:04x}{code_page:04x}\FileDescription");
        let Some(bytes) = version_value(&resource, &query, 2) else {
            continue;
        };
        if let Some(description) = readable_description(bytes) {
            return Some(description);
        }
    }
    None
}

fn version_value<'a>(resource: &'a [u8], query: &str, unit_width: usize) -> Option<&'a [u8]> {
    let query = wide(OsStr::new(query));
    let mut pointer = null_mut();
    let mut units = 0;
    if !unsafe {
        VerQueryValueW(
            resource.as_ptr().cast(),
            PCWSTR(query.as_ptr()),
            &mut pointer,
            &mut units,
        )
    }
    .as_bool()
        || pointer.is_null()
        || units == 0
    {
        return None;
    }
    // The API returns pointers into this resource block. Validate both ends
    // before accessing it, and decode bytes without assuming WORD alignment.
    let offset = (pointer as usize).checked_sub(resource.as_ptr() as usize)?;
    let length = (units as usize).checked_mul(unit_width)?;
    resource.get(offset..offset.checked_add(length)?)
}

fn readable_description(bytes: &[u8]) -> Option<String> {
    if bytes.len() > MAX_DESCRIPTION_UTF16_UNITS * 2 || !bytes.len().is_multiple_of(2) {
        return None;
    }
    let utf16 = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect::<Vec<_>>();
    let text = String::from_utf16(&utf16).ok()?;
    let description = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!description.is_empty() && !description.chars().any(char::is_control)).then_some(description)
}

fn extract(identity: &str) -> Option<Image> {
    let path = local_executable_path(identity)?;
    let mut raw_icon = HICON::default();
    let status = unsafe {
        SHDefExtractIconW(
            PCWSTR(path.as_ptr()),
            0,
            0,
            Some(&mut raw_icon),
            None,
            ICON_SIZE,
        )
    };
    // Even an unsuccessful extraction may have written a handle. Install its
    // guard before inspecting the result so every exit releases it.
    let icon = OwnedIcon(raw_icon);
    if status.is_err() || icon.0.is_invalid() {
        return None;
    }

    let mut surface = Surface::new()?;
    let black = surface.draw_on(icon.0, 0)?;
    let white = surface.draw_on(icon.0, 255)?;
    let rgba = recover_premultiplied_rgba(&black, &white);
    if rgba.chunks_exact(4).all(|pixel| pixel[3] == 0) {
        return None;
    }
    let pixels = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&rgba, ICON_SIZE, ICON_SIZE);
    Some(Image::from_rgba8_premultiplied(pixels))
}

fn local_executable_path(identity: &str) -> Option<Vec<u16>> {
    let value = identity.strip_prefix("path:")?;
    if value.contains('\0') {
        return None;
    }
    let path = Path::new(value);
    if !path
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    {
        return None;
    }
    let mut components = path.components();
    let Component::Prefix(prefix) = components.next()? else {
        return None;
    };
    let drive = match prefix.kind() {
        Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
        // UNC, verbatim UNC and device namespaces are not local file paths.
        _ => return None,
    };
    if components.next()? != Component::RootDir {
        return None;
    }
    let root = wide(OsStr::new(&format!("{}:\\", char::from(drive))));
    match unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) } {
        DRIVE_FIXED | DRIVE_REMOVABLE | DRIVE_CDROM | DRIVE_RAMDISK => {}
        _ => return None,
    }

    let mut checked = PathBuf::from(prefix.as_os_str());
    checked.push(Component::RootDir.as_os_str());
    let mut last_attributes = FILE_ATTRIBUTE_DIRECTORY.0;
    for component in components {
        let Component::Normal(part) = component else {
            return None;
        };
        checked.push(part);
        let candidate = wide(checked.as_os_str());
        let attributes = unsafe { GetFileAttributesW(PCWSTR(candidate.as_ptr())) };
        // Check from the drive down, refusing a junction/link before visiting
        // children. This also avoids fetching cloud/offline executable content.
        if attributes == INVALID_FILE_ATTRIBUTES
            || attributes & (FILE_ATTRIBUTE_REPARSE_POINT.0 | FILE_ATTRIBUTE_OFFLINE.0) != 0
        {
            return None;
        }
        last_attributes = attributes;
    }
    if last_attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
        return None;
    }
    Some(wide(checked.as_os_str()))
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

/// A modern icon uses source alpha, whereas older icons use an AND mask and
/// often leave the DIB alpha bytes unset. Drawing on opaque black and white
/// supports both: white - black = 255 * (1 - alpha), and the black rendering's
/// channels are already premultiplied. Do not trust DrawIconEx's output alpha.
/// Destination-inverting monochrome pixels have no standalone RGBA equivalent;
/// those rare pixels resolve against the black rendering.
fn recover_premultiplied_rgba(black: &[u8], white: &[u8]) -> Vec<u8> {
    debug_assert_eq!(black.len(), white.len());
    debug_assert_eq!(black.len() % 4, 0);
    let mut rgba = Vec::with_capacity(black.len());
    for (black, white) in black.chunks_exact(4).zip(white.chunks_exact(4)) {
        let difference = white[0]
            .saturating_sub(black[0])
            .max(white[1].saturating_sub(black[1]))
            .max(white[2].saturating_sub(black[2]));
        let alpha = 255 - difference;
        rgba.extend_from_slice(&[
            black[2].min(alpha),
            black[1].min(alpha),
            black[0].min(alpha),
            alpha,
        ]);
    }
    rgba
}

struct OwnedIcon(HICON);

impl Drop for OwnedIcon {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { DestroyIcon(self.0) };
        }
    }
}

struct OwnedDc(HDC);

impl Drop for OwnedDc {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { DeleteDC(self.0) };
        }
    }
}

struct OwnedBitmap(HBITMAP);

impl Drop for OwnedBitmap {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { DeleteObject(self.0.into()) };
        }
    }
}

struct Surface {
    // Surface::drop restores the previous selection first. The DC is then
    // dropped before the bitmap, also covering a failed restoration.
    dc: OwnedDc,
    _bitmap: OwnedBitmap,
    previous: HGDIOBJ,
    bits: NonNull<u8>,
}

impl Surface {
    fn new() -> Option<Self> {
        let dc = OwnedDc(unsafe { CreateCompatibleDC(None) });
        if dc.0.is_invalid() {
            return None;
        }
        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = std::mem::size_of_val(&info.bmiHeader) as u32;
        info.bmiHeader.biWidth = ICON_SIZE as i32;
        info.bmiHeader.biHeight = -(ICON_SIZE as i32);
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB.0;
        info.bmiHeader.biSizeImage = BYTE_COUNT as u32;
        let mut bits = null_mut();
        let bitmap = OwnedBitmap(
            unsafe { CreateDIBSection(Some(dc.0), &info, DIB_RGB_COLORS, &mut bits, None, 0) }
                .ok()?,
        );
        if bitmap.0.is_invalid() {
            return None;
        }
        let bits = NonNull::new(bits.cast::<u8>())?;
        let previous = unsafe { SelectObject(dc.0, bitmap.0.into()) };
        if previous.is_invalid() {
            return None;
        }
        Some(Self {
            dc,
            _bitmap: bitmap,
            previous,
            bits,
        })
    }

    fn draw_on(&mut self, icon: HICON, background: u8) -> Option<Vec<u8>> {
        // Synchronize GDI before accessing DIB memory directly, including when
        // reusing the surface for the second background.
        if !unsafe { GdiFlush() }.as_bool() {
            return None;
        }
        {
            let pixels = unsafe { std::slice::from_raw_parts_mut(self.bits.as_ptr(), BYTE_COUNT) };
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.copy_from_slice(&[background, background, background, 255]);
            }
        }
        unsafe {
            DrawIconEx(
                self.dc.0,
                0,
                0,
                icon,
                ICON_SIZE as i32,
                ICON_SIZE as i32,
                0,
                None,
                DI_NORMAL,
            )
        }
        .ok()?;
        if !unsafe { GdiFlush() }.as_bool() {
            return None;
        }
        Some(unsafe { std::slice::from_raw_parts(self.bits.as_ptr(), BYTE_COUNT) }.to_vec())
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        let _ = unsafe { SelectObject(self.dc.0, self.previous) };
    }
}

#[cfg(test)]
mod tests {
    use super::{display_name, readable_description, recover_premultiplied_rgba};

    #[test]
    fn missing_metadata_does_not_cache_an_outdated_stored_name() {
        let identity = "aumi:metadata-fallback-test";
        assert_eq!(display_name(identity, "原名称"), "原名称");
        assert_eq!(display_name(identity, "已修改名称"), "已修改名称");
    }

    #[test]
    fn file_description_preserves_unicode_and_rejects_invalid_utf16() {
        let bytes = "  应用名称\t测试 \0"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        assert_eq!(
            readable_description(&bytes).as_deref(),
            Some("应用名称 测试")
        );
        assert!(readable_description(&[0x00, 0xd8, 0, 0]).is_none());
        assert!(readable_description(&[0, 0]).is_none());
    }

    #[test]
    fn transparent_opaque_and_half_alpha_pixels_keep_rgba_order_and_premultiplication() {
        let black = [0, 0, 0, 0, 11, 22, 33, 0, 16, 32, 64, 255];
        let white = [255, 255, 255, 0, 11, 22, 33, 0, 143, 159, 191, 255];
        assert_eq!(
            recover_premultiplied_rgba(&black, &white),
            [0, 0, 0, 0, 33, 22, 11, 255, 64, 32, 16, 128],
        );
    }
}
