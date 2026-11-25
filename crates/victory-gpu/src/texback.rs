//! Adapted from Geckomacabre/vx-layer eee5667d (texture copy-back).
//! Textures shaders write that the title's own code then reads on the CPU.
//! On the console the CPU and the GPU share memory, so what a compute pass
//! stores into a texture is in title memory for the CPU to read. Here it stays
//! in the texture's host copy. The title's ocean is one: its wave passes write
//! a 256x256 height map (R32_FLOAT) and a displacement map (R32G32_FLOAT), and
//! its CPU code samples both, which is what boats float on; it read memory
//! nothing had written (run rw1, `VICTORY_READ_WATCH=1`: about 30 places in
//! the title read those two kinds), and boats sank.
//!
//! For textures of the kinds in `VICTORY_TEXTURE_BACK` (comma-separated
//! `<width>x<height>f<format>`; off unless set; `1` selects the kinds the read
//! watch found the title reading), a list in which shaders write one copies it
//! into a read-back buffer of ours when it closes, and once the list has run
//! the texels are tiled into the texture's title memory in the console's
//! layout, as the console's GPU would have left them.
//!
//! shortcut: by kind, not by texture: textures of those kinds the title never
//! reads are copied back too (about a megabyte per such list). The copy shows
//! what the list's last write left; a title read in between sees the frame
//! before, as it would before the console's GPU has run.

use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;

use crate::object::This;

/// The kinds copied back: (width, height, format).
fn kinds() -> &'static [(u64, u32, u32)] {
    static KINDS: OnceLock<Vec<(u64, u32, u32)>> = OnceLock::new();
    KINDS.get_or_init(|| {
        const DEFAULT: &str = "256x256f41,256x256f16,64x64f16,128x128f36";
        let list = match std::env::var("VICTORY_TEXTURE_BACK").as_deref() { Ok("1") => DEFAULT.to_owned(), Ok(list) => list.to_owned(), Err(_) => String::new() };
        let kinds: Vec<(u64, u32, u32)> = list
            .split(',')
            .filter_map(|k| {
                let (size, format) = k.trim().split_once('f')?;
                let (w, h) = size.split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?, format.parse().ok()?))
            })
            .collect();
        if !kinds.is_empty() {
            tracing::info!(?kinds, "textures of these kinds that shaders write are copied back into title memory (VICTORY_TEXTURE_BACK)");
        }
        kinds
    })
}

/// Whether title texture `resource` is of a kind copied back.
pub(crate) fn wanted(resource: This) -> bool {
    let kinds = kinds();
    !kinds.is_empty()
        && crate::resource::texture_kind(resource).is_some_and(|k| kinds.contains(&k))
}

// Track only outstanding water copies. Unrelated streaming changes do not invalidate
// the ticket. A remap away and back to the same address still retires it.
struct Ticket {
    owner: usize,
    at: usize,
    end: usize,
    valid: Mutex<bool>,
}
static ACTIVE: AtomicBool = AtomicBool::new(false);
static TICKETS: Mutex<Vec<Weak<Ticket>>> = Mutex::new(Vec::new());

pub(crate) fn has_pending() -> bool { ACTIVE.load(Ordering::Acquire) }

fn ticket(owner: usize, at: usize, size: usize) -> Option<Arc<Ticket>> {
    if at == 0 || size == 0 { return None; }
    let value = Arc::new(Ticket { owner, at, end: at.checked_add(size)?, valid: Mutex::new(true) });
    let mut tickets = TICKETS.lock().unwrap_or_else(|e| e.into_inner());
    tickets.retain(|item| item.strong_count() > 0);
    tickets.push(Arc::downgrade(&value));
    ACTIVE.store(true, Ordering::Release);
    Some(value)
}

fn retire_if(predicate: impl Fn(&Ticket) -> bool) {
    if !has_pending() { return; }
    let mut tickets = TICKETS.lock().unwrap_or_else(|e| e.into_inner());
    tickets.retain(|item| {
        let Some(t) = item.upgrade() else { return false };
        if predicate(&t) {
            *t.valid.lock().unwrap_or_else(|e| e.into_inner()) = false;
            return false;
        }
        true
    });
    ACTIVE.store(!tickets.is_empty(), Ordering::Release);
}

pub(crate) fn invalidate(at: usize, len: usize) {
    if len != 0 { retire_if(|t| at < t.end && t.at < at.saturating_add(len)); }
}

pub(crate) fn retire_owner(owner: usize) { retire_if(|t| t.owner == owner); }

// One counter per configured title format and size. Host footprints can use a typeless format.
pub(super) fn land_count(kind: (u64, u32, u32)) -> Option<&'static AtomicU64> {
    static COUNTS: OnceLock<Vec<AtomicU64>> = OnceLock::new();
    let index = kinds().iter().position(|&k| k == kind)?;
    Some(&COUNTS.get_or_init(|| kinds().iter().map(|_| AtomicU64::new(0)).collect())[index])
}

pub(crate) struct Pending {
    pub(crate) copy: Arc<crate::resource::HostTexture>,
    kind: (u64, u32, u32),
    layout: victory_formats::texture::Layout,
    ticket: Arc<Ticket>,
    size: usize,
}

impl Pending {
    pub(crate) fn new(resource: This) -> Option<Self> {
        let (layout, at, size) = crate::resource::title_texture(resource)?;
        let copy = crate::resource::host_copy(resource)?;
        let ticket = ticket(resource as usize, at, size)?;
        let kind = crate::resource::texture_kind(resource)?;
        Some(Self { copy, kind, layout, ticket, size })
    }

    pub(crate) fn matches(&self, resource: This) -> bool {
        self.ticket.owner == resource as usize && *self.ticket.valid.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A texture's copy into a read-back buffer, recorded into a list, and where
/// its texels go once the list has run.
pub(crate) struct Back {
    kind: (u64, u32, u32),
    buffer: ID3D12Resource,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    mips: u32,
    layout: victory_formats::texture::Layout,
    /// The texture's title memory and its layout's bytes.
    ticket: Arc<Ticket>,
    total: usize,
    size: usize,
}

/// Records into `list` the copy of `texture` (title resource `resource`'s
/// host copy, in the copy-source state) into a read-back buffer.
pub(crate) fn record(list: &ID3D12GraphicsCommandList, pending: Pending) -> Option<Back> {
    let host = crate::host::host()?;
    let Pending { copy, kind, layout, ticket, size } = pending;
    let texture = &copy.texture;
    if !*ticket.valid.lock().unwrap_or_else(|e| e.into_inner()) { skipped("destination replaced before close"); return None; }
    // SAFETY: a live texture.
    let desc = unsafe { texture.GetDesc() };
    let mips = u32::from(desc.MipLevels.max(1));
    let subresources = mips * u32::from(desc.DepthOrArraySize.max(1));
    let n = subresources as usize;
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); n];
    let mut rows = vec![0u32; n];
    let mut total = 0u64;
    // SAFETY: out arrays of `n` entries.
    unsafe {
        host.device.GetCopyableFootprints(
            &desc,
            0,
            subresources,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        )
    };
    let buffer = host.read_back_buffer(total)?;
    for (i, footprint) in footprints.iter().enumerate() {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(&buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *footprint },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: i as u32 },
        };
        // SAFETY: an open list; the texture is in the copy-source state.
        unsafe { list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    COPIED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(Back { kind, buffer, footprints, rows, mips, layout, ticket, total: total.try_into().ok()?, size })
}

/// Textures copied back since the start.
pub(crate) static COPIED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

static SKIPPED: AtomicU64 = AtomicU64::new(0);

fn skipped(reason: &str) {
    let count = SKIPPED.fetch_add(1, Ordering::Relaxed) + 1;
    if count == 1 || count.is_multiple_of(256) {
        tracing::info!(count, reason, "water texture copy skipped");
    }
}

/// Once the list that recorded `back` has run: its texels, tiled into the
/// title's layout, into the texture's title memory.
pub(crate) fn land(back: &Back) {
    static LANDED: AtomicU64 = AtomicU64::new(0);

    let at = back.ticket.at;
    let Some(_mapping) = crate::watch::title_memory_reading_now() else {
        skipped("mapping in progress");
        return;
    };
    let valid = back.ticket.valid.lock().unwrap_or_else(|e| e.into_inner());
    if !*valid {
        skipped("destination replaced");
        return;
    }
    let mut tiled = vec![0u8; back.size];
    if !crate::host::read_into(at as *const u8, &mut tiled) { skipped("destination unreadable"); return; }
    let mut data = std::ptr::null_mut();
    // SAFETY: our read-back buffer, written by the list that has run.
    unsafe {
        if back.buffer.Map(0, None, Some(&mut data)).is_err() || data.is_null() {
            return;
        }
    }
    for (i, (footprint, &rows)) in back.footprints.iter().zip(&back.rows).enumerate() {
        let (mip, slice) = (i as u32 % back.mips, i as u32 / back.mips);
        let f = footprint.Footprint;
        let pitch = f.RowPitch as usize;
        // The footprint ends at the last row's actual bytes; its trailing
        // alignment padding need not be present in the readback allocation.
        let len = (pitch * rows as usize * f.Depth as usize).min(back.total.saturating_sub(footprint.Offset as usize));
        // SAFETY: the subresource's part of the mapped buffer.
        let linear = unsafe { std::slice::from_raw_parts(data.cast::<u8>().add(footprint.Offset as usize), len) };
        if let Err(e) = back.layout.tile(&mut tiled, mip, slice, linear, pitch, pitch * rows as usize) {
            // Count each layout separately so a frequent failure cannot hide a different water map.
            type LayoutKey = (u32, u32, i32, u32, u32, u32, u32);
            static FAILURES: OnceLock<Mutex<std::collections::HashMap<LayoutKey, u64>>> = OnceLock::new();
            let d = &back.layout.desc;
            let key = (d.width, d.height, f.Format.0, d.swizzle.0, d.samples, mip, slice);
            let count = {
                let mut failures = FAILURES.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
                let n = failures.entry(key).or_default();
                *n += 1;
                *n
            };
            if count == 1 || count.is_multiple_of(256) {
                tracing::warn!(?e, count, width=d.width, height=d.height, format=f.Format.0,
                    swizzle=d.swizzle.0, samples=d.samples, ?d.dimension, mip, slice,
                    address=format_args!("{at:#x}"), title_format=back.kind.2, bytes=back.size, pitch, "a texture copied back could not be tiled");
            }
            // SAFETY: mapped above.
            unsafe { back.buffer.Unmap(0, None) };
            return;
        }
    }
    // SAFETY: mapped above.
    unsafe { back.buffer.Unmap(0, None) };
    crate::watch::writing(at, back.size);
    if crate::host::write_texture_back(at, &tiled) {
        if let Some(counter) = land_count(back.kind) {
            let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 1 || n.is_multiple_of(64) {
                tracing::info!(count=n, width=back.kind.0, height=back.kind.1, title_format=back.kind.2,
                    address=format_args!("{at:#x}"), "water texture copies landed for this kind");
            }
        }
        let count = LANDED.fetch_add(1, Ordering::Relaxed) + 1;
        if count == 1 || count.is_multiple_of(256) {
            tracing::info!(count, skipped=SKIPPED.load(Ordering::Relaxed), bytes=back.size, "water texture copy landed");
        }
    }
}
