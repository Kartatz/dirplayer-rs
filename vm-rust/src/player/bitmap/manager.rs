use std::collections::HashMap;

use super::bitmap::{decompress_bitmap, decode_jpeg_bitmap, Bitmap, PaletteRef, PendingBitmap};

/// Read-only bitmap metadata (no decode trigger).
#[derive(Clone)]
pub struct BitmapMeta {
    pub width: u16,
    pub height: u16,
    pub bit_depth: u8,
    pub original_bit_depth: u8,
    pub use_alpha: bool,
    pub pending: bool,
    pub version: u32,
    pub palette_ref: PaletteRef,
}

pub type BitmapRef = u32;
pub const INVALID_BITMAP_REF: BitmapRef = 0;

pub struct BitmapManager {
    bitmaps: HashMap<BitmapRef, Bitmap>,
    ref_counter: BitmapRef,
    /// Side table for ephemeral bitmaps — those produced by Lingo getters
    /// like `(the stage).image`, `image(w, h, d)`, `bitmap.duplicate()`,
    /// member `.image` accessors, etc. The value is the number of
    /// `Datum::BitmapRef` arena entries currently pointing at the bitmap;
    /// when it drops to zero the bitmap is freed.
    ///
    /// Cast-member-owned bitmaps are NOT in this map and are never freed by
    /// the refcount path — they live as long as the cast member does.
    ephemeral_refs: HashMap<BitmapRef, u32>,
}

impl BitmapManager {
    pub fn new() -> Self {
        Self {
            bitmaps: HashMap::new(),
            ref_counter: 0,
            ephemeral_refs: HashMap::new(),
        }
    }

    /// Drop every stored bitmap when switching movies. Cast-member-owned
    /// (anchored) bitmaps are never removed by the ephemeral refcount path, so
    /// without this they orphan here forever: loading a new movie replaces the
    /// cast list but leaks the previous movie's bitmaps (Infestation's ~363
    /// bitmaps, ~10 MB+ decoded, on every load — memory that never comes back).
    /// `ref_counter` is NOT reset so freshly-issued refs can't collide with any
    /// `Datum::BitmapRef` that a persisted global still holds.
    pub fn clear_movie_bitmaps(&mut self) {
        self.bitmaps.clear();
        self.ephemeral_refs.clear();
    }

    /// Register an anchored bitmap (owned by a cast member or other long-lived
    /// holder). Will not be auto-freed when DatumRefs drop.
    pub fn add_bitmap(&mut self, bitmap: Bitmap) -> BitmapRef {
        self.ref_counter += 1;

        let bitmap_ref = self.ref_counter;
        self.bitmaps.insert(bitmap_ref, bitmap);
        bitmap_ref
    }

    /// Register an ephemeral bitmap. Once the last `Datum::BitmapRef(N)`
    /// arena entry is dropped, the bitmap is freed. Use for `(the stage)
    /// .image`, `image(w, h, d)`, `bitmap.duplicate()`, member `.image`
    /// snapshots — anywhere a Lingo expression produces a bitmap with no
    /// other persistent owner.
    pub fn add_ephemeral_bitmap(&mut self, bitmap: Bitmap) -> BitmapRef {
        self.ref_counter += 1;

        let bitmap_ref = self.ref_counter;
        self.bitmaps.insert(bitmap_ref, bitmap);
        // Start at 0 — the caller's `alloc_datum(Datum::BitmapRef(...))` will
        // bump it via `incref_ephemeral`. If for some reason the bitmap is
        // never wrapped in a DatumRef the entry leaks, but that's rare and
        // strictly better than the previous always-leak behaviour.
        self.ephemeral_refs.insert(bitmap_ref, 0);
        bitmap_ref
    }

    pub fn replace_bitmap(&mut self, bitmap_ref: BitmapRef, mut bitmap: Bitmap) {
        // Increment version to indicate the bitmap has changed
        // This allows texture caches to know when to re-upload
        if let Some(old_bitmap) = self.bitmaps.get(&bitmap_ref) {
            bitmap.version = old_bitmap.version.wrapping_add(1);
        }
        self.bitmaps.insert(bitmap_ref, bitmap);
    }

    /// Read-only access that does NOT trigger a lazy decode. Returns the
    /// bitmap as-registered: dimensions and (for already-decoded bitmaps)
    /// pixel data. Callers must not rely on `data` being populated when
    /// `get_bitmap_meta().pending` is true.
    pub fn get_bitmap_static(&self, bitmap_ref: BitmapRef) -> Option<&Bitmap> {
        self.bitmaps.get(&bitmap_ref)
    }

    /// Fetch a bitmap, decoding it first if it was registered as a lazily
    /// decoded cast-member bitmap. Takes `&mut self` because the decode
    /// materialises `data` in place and drops the encoded source.
    pub fn get_bitmap(&mut self, bitmap_ref: BitmapRef) -> Option<&Bitmap> {
        if self.bitmaps.get(&bitmap_ref)?.pending.is_some() {
            self.decode_pending(bitmap_ref);
        }
        self.bitmaps.get(&bitmap_ref)
    }

    /// Read-only metadata access that does NOT trigger a lazy decode.
    /// Use in read-only contexts (`&DirPlayer`) that only need dimensions
    /// or version info.
    pub fn get_bitmap_meta(&self, bitmap_ref: BitmapRef) -> Option<BitmapMeta> {
        self.bitmaps.get(&bitmap_ref).map(|b| BitmapMeta {
            width: b.width,
            height: b.height,
            bit_depth: b.bit_depth,
            original_bit_depth: b.original_bit_depth,
            use_alpha: b.use_alpha,
            pending: b.pending.is_some(),
            version: b.version,
            palette_ref: b.palette_ref.clone(),
        })
    }

    fn decode_pending(&mut self, bitmap_ref: BitmapRef) {
        let Some(pending) = self.bitmaps.get_mut(&bitmap_ref).and_then(|b| b.pending.take()) else {
            return;
        };
        let decoded = match *pending {
            PendingBitmap::Bitd { data, info, cast_lib, version } => {
                decompress_bitmap(&data, &info, cast_lib, version)
            }
            PendingBitmap::JpegWithAlfa { jpeg, alfa, info } => {
                decode_jpeg_bitmap(&jpeg, &info, Some(&alfa))
            }
            PendingBitmap::CompressedBitd { slab, offset, len, compression_id, info, cast_lib, version } => {
                match PendingBitmap::inflate_slice(&slab, offset, len, &compression_id) {
                    Ok(data) => decompress_bitmap(&data, &info, cast_lib, version),
                    Err(e) => Err(e),
                }
            }
            PendingBitmap::CompressedJpegWithAlfa { slab, jpeg_offset, jpeg_len, alfa_offset, alfa_len, compression_id, info } => {
                let jpeg_r = PendingBitmap::inflate_slice(&slab, jpeg_offset, jpeg_len, &compression_id);
                let alfa_r = if alfa_len > 0 {
                    PendingBitmap::inflate_slice(&slab, alfa_offset, alfa_len, &compression_id)
                } else {
                    Ok(Vec::new())
                };
                match (jpeg_r, alfa_r) {
                    (Ok(jpeg), Ok(alfa)) => decode_jpeg_bitmap(&jpeg, &info, Some(&alfa)),
                    (Err(e), _) | (_, Err(e)) => Err(e),
                }
            }
        };
        match decoded {
            Ok(bitmap) => {
                if let Some(existing) = self.bitmaps.get_mut(&bitmap_ref) {
                    existing.width = bitmap.width;
                    existing.height = bitmap.height;
                    existing.bit_depth = bitmap.bit_depth;
                    existing.original_bit_depth = bitmap.original_bit_depth;
                    existing.data = bitmap.data;
                    existing.palette_ref = bitmap.palette_ref;
                    existing.matte = bitmap.matte;
                    existing.use_alpha = bitmap.use_alpha;
                    existing.version = existing.version.wrapping_add(1);
                }
            }
            Err(e) => {
                log::warn!("lazy bitmap decode failed for ref {}: {:?}; clearing pending", bitmap_ref, e);
                if let Some(existing) = self.bitmaps.get_mut(&bitmap_ref) {
                    if existing.data.is_empty() {
                        existing.width = 1.max(existing.width.min(1));
                        existing.height = 1.max(existing.height.min(1));
                    }
                }
            }
        }
    }

    #[allow(dead_code)]
    pub fn get_bitmap_mut(&mut self, bitmap_ref: BitmapRef) -> Option<&mut Bitmap> {
        // Increment version when giving mutable access, as the bitmap may be modified
        // This ensures texture caches know to re-upload the texture
        if let Some(bitmap) = self.bitmaps.get_mut(&bitmap_ref) {
            bitmap.version = bitmap.version.wrapping_add(1);
            Some(bitmap)
        } else {
            None
        }
    }

    /// Bump the ephemeral refcount for `bitmap_ref`. No-op for anchored
    /// bitmaps (those not in `ephemeral_refs`). Called by the allocator
    /// when a new arena entry wrapping `Datum::BitmapRef(N)` is created.
    pub fn incref_ephemeral(&mut self, bitmap_ref: BitmapRef) {
        if let Some(count) = self.ephemeral_refs.get_mut(&bitmap_ref) {
            *count = count.saturating_add(1);
        }
    }

    /// Decrement the ephemeral refcount. If it reaches zero the bitmap and
    /// its tracking entry are removed. No-op for anchored bitmaps.
    pub fn decref_ephemeral(&mut self, bitmap_ref: BitmapRef) {
        let should_free = if let Some(count) = self.ephemeral_refs.get_mut(&bitmap_ref) {
            *count = count.saturating_sub(1);
            *count == 0
        } else {
            false
        };
        if should_free {
            self.ephemeral_refs.remove(&bitmap_ref);
            self.bitmaps.remove(&bitmap_ref);
        }
    }
}
