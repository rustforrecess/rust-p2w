//! Bare-metal runtime for the rust-p2w native (Pico 2 W) backend.
//!
//! This implements the `p2w_*` ABI the LLVM emitter (`rust-p2w/src/llvm.rs`)
//! calls — see `PICO_BACKEND.md`. The emitter is representation-agnostic; **this
//! crate owns the value representation and the allocator.** It's `no_std` for the
//! device (`thumbv8m.main-none-eabihf`) and compiled to a static library that
//! links against the emitted IR in the toolchain phase. The logic is pure, so it
//! host-tests under plain `cargo test`.
//!
//! ## Value representation — tagged `i32` (2-bit tag)
//! A Python value is one 32-bit word (pointer-width on the RP2350; matches
//! MicroPython/V8-SMI — see the value-model note):
//! - `0b01` → **small int**, payload `v >> 2` (signed, 30-bit; bignum promotion
//!   on overflow is a TODO).
//! - `0b00` → **heap pointer** (objects are ≥4-byte aligned, so the low bits are
//!   free); the object header carries a type tag (str/list/dict/float). An
//!   `f64` can't be inline (too wide for the tagged `i32`), so floats are boxed.
//! - `0b10` → **immediate singleton**: `None`, `False`, `True`.
//!
//! Implemented: int/bool/None/float values, arithmetic (`+ - * / // % **`, neg)
//! with int→float promotion, comparisons, truthiness, `not`, `print`, and the
//! heap types (strings/lists/dicts/iterators) on a bump+free-list allocator with
//! ref-counting. Float `**`/formatting use the no_std `libm` crate (also device-
//! ready). TODO: bignum promotion, scientific-notation float repr.

#![cfg_attr(not(test), no_std)]

/// An encoded Python value.
pub type Value = i32;

const TAG_MASK: i32 = 0b11;
/// Heap-pointer tag — used once the heap value types (str/list/dict) land.
#[allow(dead_code)]
const TAG_PTR: i32 = 0b00;
const TAG_INT: i32 = 0b01;
const TAG_SPECIAL: i32 = 0b10;

/// Immediate singletons (kind in the upper bits, `TAG_SPECIAL` in the low two).
const V_NONE: Value = TAG_SPECIAL; // kind 0
const V_FALSE: Value = (1 << 2) | TAG_SPECIAL; // kind 1
const V_TRUE: Value = (2 << 2) | TAG_SPECIAL; // kind 2

/// 30-bit small-int range (bignum promotion beyond this is a TODO).
const INT_MIN: i64 = -(1 << 29);
const INT_MAX: i64 = (1 << 29) - 1;

fn is_int(v: Value) -> bool {
    v & TAG_MASK == TAG_INT || (is_heap(v) && obj_tag(v) == T_INT)
}

fn as_int(v: Value) -> i64 {
    if v & TAG_MASK == TAG_INT {
        (v >> 2) as i64 // inline 30-bit small int
    } else {
        rd((v as usize) + 8) as i32 as i64 // boxed full-width i32
    }
}

/// Allocate a boxed full-width `i32` (for values outside the inline range).
fn int_alloc(n: i32) -> Value {
    let p = alloc(8 + 4); // [tag][rc][i32]
    if p == 0 {
        trap("out of memory");
    }
    wr(p, T_INT);
    wr(p + 4, 1);
    live_inc();
    wr(p + 8, n as u32);
    p as Value
}

/// Encode an integer. Wraps to `i32` (the value model's int width), then uses the
/// inline 30-bit form when it fits, else a heap box — never truncates the i32.
fn make_int(n: i64) -> Value {
    let n = n as i32; // i32 wraparound, matching native unboxed-int arithmetic
    if (INT_MIN..=INT_MAX).contains(&(n as i64)) {
        ((n) << 2) | TAG_INT
    } else {
        int_alloc(n)
    }
}

fn make_bool(b: bool) -> Value {
    if b { V_TRUE } else { V_FALSE }
}

/// Numeric view of int/bool (Python treats `True`/`False` as `1`/`0`), or None
/// for non-numeric values.
fn num(v: Value) -> Option<i64> {
    if is_int(v) {
        Some(as_int(v))
    } else if v == V_TRUE {
        Some(1)
    } else if v == V_FALSE {
        Some(0)
    } else {
        None
    }
}

// --- heap: a static arena + first-fit free list ----------------------------
//
// Heap values are tagged pointers (`TAG_PTR`, low 2 bits 0): the value IS a
// 4-byte-aligned **offset** into the arena (an offset, not a raw machine
// pointer, so it fits `i32` identically on the 32-bit device and a 64-bit test
// host). Object layout at offset `p`: [tag u32][refcount u32][len u32][data...].
// Allocation bumps a cursor; `free` pushes whole blocks onto a first-fit free
// list (block size is kept in a u32 header just before the payload). Reset-per-
// run reclaims everything at once; ref counting (retain/release) reclaims
// mid-run. Strings are the first heap type; lists/dicts follow.

const HEAP_SIZE: usize = 64 * 1024; // device build tunes this to available SRAM

#[unsafe(no_mangle)]
static mut HEAP: [u8; HEAP_SIZE] = [0; HEAP_SIZE];
static mut CURSOR: usize = 4; // offset 0 is reserved (never a valid object)
static mut FREELIST: usize = 0; // head block offset, 0 = empty
// Live heap-object count (births − frees). The RC acceptance gate: a finished
// program with balanced retain/release must end at 0. See tools/native_run.sh.
static mut LIVE: i32 = 0;
// Cumulative allocation count (objects + buffers). Lets the run-oracle measure
// the FBIP drop-reuse win (an in-place map does zero allocations).
static mut ALLOCS: i32 = 0;
/// High-water mark of `LIVE` — peak simultaneous heap objects. The metric that
/// precise (last-use) drops improve: total allocations stay the same, but values
/// die sooner, so fewer are ever alive at once. Resets with the heap.
static mut PEAK: i32 = 0;

/// Object type tags (in the header's first word).
const T_STR: u32 = 1;
/// A boxed `f64`: layout `[tag][rc][f64 (8 bytes)]`. Floats can't be inline —
/// an `f64` doesn't fit a tagged `i32` — so they're heap values, matching the
/// browser backend's `$FLOAT` struct (the two backends stay at parity).
const T_FLOAT: u32 = 5;
/// A boxed full-width `i32`: layout `[tag][rc][i32]`. Used when an int doesn't
/// fit the 30-bit inline range, so the int type covers the whole `i32` (matching
/// the value model's unboxed `i32`) instead of silently truncating.
const T_INT: u32 = 6;

fn heap_base() -> *mut u8 {
    &raw mut HEAP as *mut u8
}

fn rd(off: usize) -> u32 {
    unsafe { core::ptr::read_unaligned(heap_base().add(off) as *const u32) }
}
fn wr(off: usize, v: u32) {
    unsafe { core::ptr::write_unaligned(heap_base().add(off) as *mut u32, v) }
}
fn rd_byte(off: usize) -> u8 {
    unsafe { *heap_base().add(off) }
}
fn wr_byte(off: usize, b: u8) {
    unsafe { *heap_base().add(off) = b }
}

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// Reset the arena (run-to-completion reclamation; also resets between tests).
pub fn heap_reset() {
    unsafe {
        CURSOR = 4;
        FREELIST = 0;
        LIVE = 0;
        ALLOCS = 0;
        PEAK = 0;
    }
}

/// One heap object was born (refcount initialized to 1).
fn live_inc() {
    unsafe {
        LIVE += 1;
        if LIVE > PEAK {
            PEAK = LIVE;
        }
    }
}
/// One heap object was freed.
fn live_dec() {
    unsafe { LIVE -= 1 }
}

/// Number of live heap objects right now (births − frees). The run-oracle calls
/// this at program exit: a leak-free program with correct RC returns 0.
#[unsafe(no_mangle)]
pub extern "C" fn p2w_live() -> i32 {
    unsafe { LIVE }
}

/// Extract a raw `i32` from a boxed value — the unbox half of the value model's
/// boundary (the box half is `p2w_int`). Traps on a non-int (a runtime
/// TypeError). See VALUE_MODEL.md.
#[unsafe(no_mangle)]
pub extern "C" fn p2w_unbox_int(v: Value) -> i32 {
    if !is_int(v) {
        trap("expected an int");
    }
    as_int(v) as i32
}

/// Cumulative allocations so far (objects + buffers). Resets with the heap.
#[unsafe(no_mangle)]
pub extern "C" fn p2w_allocs() -> i32 {
    unsafe { ALLOCS }
}

/// Peak simultaneous live heap objects (the `LIVE` high-water mark) — what
/// precise drops shrink on a memory-tight device. Resets with the heap.
#[unsafe(no_mangle)]
pub extern "C" fn p2w_peak() -> i32 {
    unsafe { PEAK }
}

/// Whether `v` is a heap object with refcount 1 (uniquely owned) — the FBIP
/// reuse test: a unique value can be mutated in place since no one else can
/// observe it. Inline/non-heap values are never "unique" (nothing to reuse).
#[unsafe(no_mangle)]
pub extern "C" fn p2w_unique(v: Value) -> bool {
    is_heap(v) && rd(v as usize + 4) == 1
}

/// Assign-site drop-reuse guard: can `v` be overwritten in place as an
/// `n`-element collection of `tag`? Requires the right tag (a Boxed slot could
/// hold *anything* — a string must never be setindex'd), unique ownership, and
/// the exact length (the literal's element count).
fn can_reuse(v: Value, tag: u32, n: i32) -> bool {
    is_heap(v) && rd(v as usize) == tag && rd(v as usize + 4) == 1 && rd(v as usize + 8) == n as u32
}

/// `v` is a unique n-element boxed list — safe to overwrite element-wise.
#[unsafe(no_mangle)]
pub extern "C" fn p2w_can_reuse_list(v: Value, n: i32) -> bool {
    can_reuse(v, T_LIST, n)
}

/// `v` is a unique n-element packed int array — safe to overwrite in place.
#[unsafe(no_mangle)]
pub extern "C" fn p2w_can_reuse_iarray(v: Value, n: i32) -> bool {
    can_reuse(v, T_IARRAY, n)
}

/// `v` is a unique n-element packed float array — safe to overwrite in place.
#[unsafe(no_mangle)]
pub extern "C" fn p2w_can_reuse_farray(v: Value, n: i32) -> bool {
    can_reuse(v, T_FARRAY, n)
}

/// `x = x + b` — the append/extend drop-reuse. CONSUMES `a` (the slot's old
/// value: reused in place or released); BORROWS `b` (like `p2w_add`'s args).
///
/// Unique-string append writes `b`'s bytes into `a`'s spare block capacity
/// (the allocator's size header makes capacity knowable) — zero allocation —
/// or reallocates with 2× slack so subsequent appends land in place
/// (amortized O(1), the CPython refcount-1 trick). Unique-list concat pushes
/// `b`'s elements into `a`'s buffer. `a != b` guards self-append (`s + s`
/// passes the same pointer twice at rc 1); ANY other live reference implies
/// rc >= 2, so the uniqueness test alone makes aliasing fall back to the
/// plain copy path with identical semantics.
#[unsafe(no_mangle)]
pub extern "C" fn p2w_add_assign(a: Value, b: Value) -> Value {
    // Unique string append.
    if is_heap(a)
        && obj_tag(a) == T_STR
        && rd(a as usize + 4) == 1
        && a != b
        && is_heap(b)
        && obj_tag(b) == T_STR
    {
        let (la, lb) = (str_len(a), str_len(b));
        let need = 12 + la + lb;
        // Block payload capacity: the u32 size header just before the payload
        // stores align4(4 + payload), so usable payload = header - 4.
        let cap = rd(a as usize - 4) as usize - 4;
        if cap >= need {
            for i in 0..lb {
                wr_byte(a as usize + 12 + la + i, str_byte(b, i));
            }
            wr(a as usize + 8, (la + lb) as u32);
            return a; // in place: no allocation, a's bytes never copied
        }
        // Grow with slack so the next appends land in place.
        let p = alloc(need * 2);
        if p == 0 {
            trap("out of memory");
        }
        wr(p, T_STR);
        wr(p + 4, 1);
        live_inc();
        wr(p + 8, (la + lb) as u32);
        for i in 0..la {
            wr_byte(p + 12 + i, str_byte(a, i));
        }
        for i in 0..lb {
            wr_byte(p + 12 + la + i, str_byte(b, i));
        }
        p2w_release(a);
        return p as Value;
    }
    // Unique list extend: push b's elements into a's existing buffer.
    if is_heap(a)
        && obj_tag(a) == T_LIST
        && rd(a as usize + 4) == 1
        && a != b
        && is_heap(b)
        && obj_tag(b) == T_LIST
    {
        let ao = a as usize;
        for i in 0..coll_len(b as usize) {
            list_push(ao, owned(list_get(b as usize, i)));
        }
        return a;
    }
    // Fallback: plain add, then release the consumed old value (a no-op for
    // inline scalars).
    let r = p2w_add(a, b);
    p2w_release(a);
    r
}

/// `str(v)` — the value's display form as a fresh heap string. Runs the print
/// formatter twice: once to size the buffer, once to fill it (no_std, no Vec).
#[unsafe(no_mangle)]
pub extern "C" fn p2w_str_of(v: Value) -> Value {
    let mut n = 0usize;
    write_value(v, &mut |_| n += 1);
    let p = alloc(12 + n);
    if p == 0 {
        trap("out of memory");
    }
    wr(p, T_STR);
    wr(p + 4, 1);
    live_inc();
    wr(p + 8, n as u32);
    let mut i = 0usize;
    write_value(v, &mut |b| {
        wr_byte(p + 12 + i, b);
        i += 1;
    });
    p as Value
}

/// `obj[start:stop:step]` for lists and strings — Python slice semantics: each
/// bound is `V_NONE` when omitted, indices may be negative, and a negative step
/// reverses. Returns a fresh list/string (owning retained elements for a list).
#[unsafe(no_mangle)]
pub extern "C" fn p2w_slice(obj: Value, start_v: Value, stop_v: Value, step_v: Value) -> Value {
    if !(is_heap(obj) && matches!(obj_tag(obj), T_STR | T_LIST)) {
        trap("only lists and strings can be sliced");
    }
    let o = obj as usize;
    let len = rd(o + 8) as i64; // element/byte count (same offset for str and list)
    let step = if step_v == V_NONE { 1 } else { as_int(step_v) };
    if step == 0 {
        trap("slice step cannot be zero");
    }
    // CPython's PySlice_AdjustIndices for an explicit bound.
    let adjust = |raw: i64| -> i64 {
        let mut i = raw;
        if i < 0 {
            i += len;
            if i < 0 {
                i = if step < 0 { -1 } else { 0 };
            }
        } else if i >= len {
            i = if step < 0 { len - 1 } else { len };
        }
        i
    };
    let start = if start_v == V_NONE {
        if step < 0 { len - 1 } else { 0 }
    } else {
        adjust(as_int(start_v))
    };
    let stop = if stop_v == V_NONE {
        if step < 0 { -1 } else { len }
    } else {
        adjust(as_int(stop_v))
    };
    let take = |i: i64| if step > 0 { i < stop } else { i > stop };

    if obj_tag(obj) == T_STR {
        // Two passes (no_std, no Vec): size the result, then fill it.
        let mut n = 0usize;
        let mut i = start;
        while take(i) {
            n += 1;
            i += step;
        }
        let p = alloc(12 + n);
        if p == 0 {
            trap("out of memory");
        }
        wr(p, T_STR);
        wr(p + 4, 1);
        live_inc();
        wr(p + 8, n as u32);
        let mut j = 0usize;
        let mut i = start;
        while take(i) {
            wr_byte(p + 12 + j, str_byte(obj, i as usize));
            j += 1;
            i += step;
        }
        p as Value
    } else {
        let result = coll_new(T_LIST);
        let mut i = start;
        while take(i) {
            list_push(result as usize, owned(list_get(o, i as usize)));
            i += step;
        }
        result
    }
}

/// Extract a raw `f64` from a boxed value (the unbox half for floats). Accepts a
/// boxed int too (Python int→float promotion); traps otherwise.
#[unsafe(no_mangle)]
pub extern "C" fn p2w_unbox_float(v: Value) -> f64 {
    if is_float(v) {
        as_f64(v)
    } else if is_int(v) {
        as_int(v) as f64
    } else {
        trap("expected a float")
    }
}

/// Allocate `payload` bytes; returns the payload offset, or 0 on OOM. The block
/// carries a u32 size header just before the payload (for `free`).
fn alloc(payload: usize) -> usize {
    let need = align4(4 + payload);
    unsafe {
        ALLOCS += 1;
        // First-fit reuse from the free list (whole block; no splitting).
        let mut prev = 0usize;
        let mut cur = FREELIST;
        while cur != 0 {
            let bsize = rd(cur) as usize;
            let next = rd(cur + 4) as usize;
            if bsize >= need {
                if prev == 0 {
                    FREELIST = next;
                } else {
                    wr(prev + 4, next as u32);
                }
                return cur + 4;
            }
            prev = cur;
            cur = next;
        }
        // Bump.
        let blk = CURSOR;
        if blk + need > HEAP_SIZE {
            return 0;
        }
        wr(blk, need as u32);
        CURSOR = blk + need;
        blk + 4
    }
}

fn dealloc(payload_off: usize) {
    let block = payload_off - 4;
    unsafe {
        wr(block + 4, FREELIST as u32); // reuse the payload's first word as `next`
        FREELIST = block;
    }
}

fn is_heap(v: Value) -> bool {
    v != 0 && (v & TAG_MASK) == TAG_PTR
}

fn obj_tag(v: Value) -> u32 {
    rd(v as usize)
}

fn str_len(v: Value) -> usize {
    rd(v as usize + 8) as usize
}

fn str_byte(v: Value, i: usize) -> u8 {
    rd_byte(v as usize + 12 + i)
}

/// Allocate a string object from raw bytes (refcount starts at 1).
fn str_alloc(bytes: &[u8]) -> Value {
    let p = alloc(12 + bytes.len());
    if p == 0 {
        trap("out of memory");
    }
    wr(p, T_STR);
    wr(p + 4, 1);
    live_inc();
    wr(p + 8, bytes.len() as u32);
    for (i, &b) in bytes.iter().enumerate() {
        wr_byte(p + 12 + i, b);
    }
    p as Value
}

// --- floats (heap-boxed f64) -----------------------------------------------

fn is_float(v: Value) -> bool {
    is_heap(v) && obj_tag(v) == T_FLOAT
}

fn as_f64(v: Value) -> f64 {
    unsafe { core::ptr::read_unaligned(heap_base().add(v as usize + 8) as *const f64) }
}

/// Box an `f64` on the heap (refcount starts at 1).
fn float_alloc(x: f64) -> Value {
    let p = alloc(8 + 8); // [tag][rc][f64]
    if p == 0 {
        trap("out of memory");
    }
    wr(p, T_FLOAT);
    wr(p + 4, 1);
    live_inc();
    unsafe { core::ptr::write_unaligned(heap_base().add(p + 8) as *mut f64, x) };
    p as Value
}

/// Numeric-as-float view: int/bool widen to `f64`, a float yields its value,
/// anything else is non-numeric. (Our ints are ≤30-bit, so `as f64` is exact.)
fn fnum(v: Value) -> Option<f64> {
    if let Some(n) = num(v) {
        Some(n as f64)
    } else if is_float(v) {
        Some(as_f64(v))
    } else {
        None
    }
}

// --- the p2w_* ABI ---------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn p2w_int(n: i32) -> Value {
    make_int(n as i64)
}

/// Box a float literal (the emitter passes the IR `double` constant).
#[unsafe(no_mangle)]
pub extern "C" fn p2w_float(x: f64) -> Value {
    float_alloc(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_bool(b: i32) -> Value {
    make_bool(b != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_none() -> Value {
    V_NONE
}

/// Numeric binary op, trapping on non-numeric operands (heap types arrive with
/// the next slice).
fn numeric<F: Fn(i64, i64) -> i64>(a: Value, b: Value, f: F) -> Value {
    match (num(a), num(b)) {
        (Some(x), Some(y)) => make_int(f(x, y)),
        _ => trap("unsupported operand type for a numeric op (heap types are TODO)"),
    }
}

/// Numeric op with int and float variants: if either operand is a float, both
/// promote to `f64` and the result is a boxed float (Python's promotion rule);
/// otherwise it's integer arithmetic.
fn arith<FI: Fn(i64, i64) -> i64, FF: Fn(f64, f64) -> f64>(
    a: Value,
    b: Value,
    fi: FI,
    ff: FF,
) -> Value {
    if is_float(a) || is_float(b) {
        match (fnum(a), fnum(b)) {
            (Some(x), Some(y)) => float_alloc(ff(x, y)),
            _ => trap("unsupported operand type for a numeric op"),
        }
    } else {
        numeric(a, b, fi)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_add(a: Value, b: Value) -> Value {
    // String + string = concatenation; otherwise numeric.
    if is_heap(a) && obj_tag(a) == T_STR && is_heap(b) && obj_tag(b) == T_STR {
        let (la, lb) = (str_len(a), str_len(b));
        let p = alloc(12 + la + lb);
        if p == 0 {
            trap("out of memory");
        }
        wr(p, T_STR);
        wr(p + 4, 1);
        live_inc();
        wr(p + 8, (la + lb) as u32);
        for i in 0..la {
            wr_byte(p + 12 + i, str_byte(a, i));
        }
        for i in 0..lb {
            wr_byte(p + 12 + la + i, str_byte(b, i));
        }
        return p as Value;
    }
    // List + list = a new concatenated list.
    if is_heap(a) && obj_tag(a) == T_LIST && is_heap(b) && obj_tag(b) == T_LIST {
        let r = coll_new(T_LIST);
        let ro = r as usize;
        // The new list gets its own reference to each copied element.
        for i in 0..coll_len(a as usize) {
            list_push(ro, owned(list_get(a as usize, i)));
        }
        for i in 0..coll_len(b as usize) {
            list_push(ro, owned(list_get(b as usize, i)));
        }
        return r;
    }
    arith(a, b, |x, y| x + y, |x, y| x + y)
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_sub(a: Value, b: Value) -> Value {
    if both_sets(a, b) {
        return set_difference(a as usize, b as usize); // set difference `a - b`
    }
    arith(a, b, |x, y| x - y, |x, y| x - y)
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_mul(a: Value, b: Value) -> Value {
    arith(a, b, |x, y| x * y, |x, y| x * y)
}

/// True division (`/`) is *always* float in Python: `4 / 2 == 2.0`.
#[unsafe(no_mangle)]
pub extern "C" fn p2w_div(a: Value, b: Value) -> Value {
    match (fnum(a), fnum(b)) {
        (Some(x), Some(y)) => {
            if y == 0.0 {
                trap("division by zero");
            }
            float_alloc(x / y)
        }
        _ => trap("unsupported operand type for /"),
    }
}

/// Power (`**`): `int ** non-negative int` stays an exact int; everything else
/// (float operand, or negative exponent like `2 ** -1 == 0.5`) is float.
#[unsafe(no_mangle)]
pub extern "C" fn p2w_pow(a: Value, b: Value) -> Value {
    if !is_float(a)
        && !is_float(b)
        && let (Some(base), Some(exp)) = (num(a), num(b))
        && exp >= 0
    {
        let mut acc: i64 = 1;
        for _ in 0..exp {
            acc *= base;
        }
        return make_int(acc);
    }
    match (fnum(a), fnum(b)) {
        (Some(x), Some(y)) => float_alloc(libm::pow(x, y)),
        _ => trap("unsupported operand type for **"),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_floordiv(a: Value, b: Value) -> Value {
    if is_float(a) || is_float(b) {
        return match (fnum(a), fnum(b)) {
            (Some(x), Some(y)) => {
                if y == 0.0 {
                    trap("float floor division by zero");
                }
                float_alloc(libm::floor(x / y))
            }
            _ => trap("unsupported operand type for //"),
        };
    }
    match (num(a), num(b)) {
        (Some(_), Some(0)) => trap("integer division or modulo by zero"),
        (Some(x), Some(y)) => make_int(x.div_euclid(y)),
        _ => trap("unsupported operand type for //"),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_mod(a: Value, b: Value) -> Value {
    if is_float(a) || is_float(b) {
        return match (fnum(a), fnum(b)) {
            (Some(x), Some(y)) => {
                if y == 0.0 {
                    trap("float modulo by zero");
                }
                // Python's `%` takes the divisor's sign: a - floor(a/b)*b.
                float_alloc(x - libm::floor(x / y) * y)
            }
            _ => trap("unsupported operand type for %"),
        };
    }
    match (num(a), num(b)) {
        (Some(_), Some(0)) => trap("integer division or modulo by zero"),
        (Some(x), Some(y)) => make_int(x.rem_euclid(y)),
        _ => trap("unsupported operand type for %"),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_neg(a: Value) -> Value {
    if is_float(a) {
        return float_alloc(-as_f64(a));
    }
    match num(a) {
        Some(x) => make_int(-x),
        None => trap("bad operand type for unary -"),
    }
}

fn compare<FI: Fn(i64, i64) -> bool, FF: Fn(f64, f64) -> bool>(
    a: Value,
    b: Value,
    fi: FI,
    ff: FF,
) -> Value {
    if is_float(a) || is_float(b) {
        return match (fnum(a), fnum(b)) {
            (Some(x), Some(y)) => make_bool(ff(x, y)),
            _ => trap("unsupported operand type for a comparison"),
        };
    }
    match (num(a), num(b)) {
        (Some(x), Some(y)) => make_bool(fi(x, y)),
        _ => trap("unsupported operand type for a comparison"),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_lt(a: Value, b: Value) -> Value {
    compare(a, b, |x, y| x < y, |x, y| x < y)
}
#[unsafe(no_mangle)]
pub extern "C" fn p2w_le(a: Value, b: Value) -> Value {
    compare(a, b, |x, y| x <= y, |x, y| x <= y)
}
#[unsafe(no_mangle)]
pub extern "C" fn p2w_gt(a: Value, b: Value) -> Value {
    compare(a, b, |x, y| x > y, |x, y| x > y)
}
#[unsafe(no_mangle)]
pub extern "C" fn p2w_ge(a: Value, b: Value) -> Value {
    compare(a, b, |x, y| x >= y, |x, y| x >= y)
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_eq(a: Value, b: Value) -> Value {
    make_bool(value_eq(a, b))
}
#[unsafe(no_mangle)]
pub extern "C" fn p2w_ne(a: Value, b: Value) -> Value {
    make_bool(!value_eq(a, b))
}

fn value_eq(a: Value, b: Value) -> bool {
    if (is_float(a) || is_float(b))
        && let (Some(x), Some(y)) = (fnum(a), fnum(b))
    {
        return x == y; // numeric cross-type: 1 == 1.0
    }
    if let (Some(x), Some(y)) = (num(a), num(b)) {
        return x == y; // int/bool compare numerically (True == 1)
    }
    if is_heap(a) && is_heap(b) && obj_tag(a) == obj_tag(b) {
        let (oa, ob) = (a as usize, b as usize);
        match obj_tag(a) {
            T_STR => {
                let n = coll_len(oa);
                return n == coll_len(ob) && (0..n).all(|i| str_byte(a, i) == str_byte(b, i));
            }
            // Tuples compare elementwise like lists; the tag guard above already
            // makes a tuple != a list with the same elements.
            T_LIST | T_TUPLE => {
                let n = coll_len(oa);
                return n == coll_len(ob)
                    && (0..n).all(|i| value_eq(list_get(oa, i), list_get(ob, i)));
            }
            T_DICT => {
                let n = coll_len(oa);
                if n != coll_len(ob) {
                    return false;
                }
                return (0..n).all(|i| {
                    let k = dict_key(oa, i);
                    match dict_find(ob, k) {
                        Some(j) => value_eq(dict_val(oa, i), dict_val(ob, j)),
                        None => false,
                    }
                });
            }
            T_SET => {
                // Order-independent: same size and every element of `a` is in `b`.
                let n = coll_len(oa);
                return n == coll_len(ob) && (0..n).all(|i| coll_contains(ob, list_get(oa, i)));
            }
            _ => {}
        }
    }
    a == b // None == None; identical specials/heap identity
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_truthy(v: Value) -> bool {
    if let Some(n) = num(v) {
        return n != 0;
    }
    if is_float(v) {
        return as_f64(v) != 0.0; // 0.0 and -0.0 are falsy
    }
    if is_heap(v) && matches!(obj_tag(v), T_STR | T_LIST | T_DICT | T_TUPLE) {
        return coll_len(v as usize) != 0; // empty string/list/dict/tuple is falsy
    }
    v != V_NONE // None is falsy; other heap values are truthy
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_not(v: Value) -> Value {
    make_bool(!p2w_truthy(v))
}

// --- heap value ABI (strings; lists/dicts follow) --------------------------

/// Build a string value from a UTF-8 buffer (the emitter passes a pointer to a
/// private constant + its length).
///
/// # Safety
/// `ptr` must point to `len` valid bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn p2w_str(ptr: *const u8, len: i32) -> Value {
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len as usize) };
    str_alloc(bytes)
}

// p2w_len / p2w_index / p2w_setindex dispatch over str/list/dict — defined in
// the container section below.

// Refcount inc/dec — the **atomicity swap point**. Single-threaded today
// (non-atomic). For micro-ROS / an RTOS where a value can be shared across
// tasks, these must become atomic (e.g. CAS) and the arena must be locked; the
// rest of the RC machinery is unaffected. The intended invariant is instead to
// keep the interpreter single-threaded and *marshal* values (copy) at the ROS
// boundary — see MEMORY_MANAGEMENT.md.
fn rc_inc(o: usize) {
    wr(o + 4, rd(o + 4) + 1);
}
fn rc_dec(o: usize) -> u32 {
    let n = rd(o + 4) - 1;
    wr(o + 4, n);
    n
}

/// Return `v` as an **owned** reference (retaining if it's a heap value), so a
/// borrowed element handed out by `p2w_index`/iteration becomes the caller's to
/// release. Inline values pass through unchanged.
fn owned(v: Value) -> Value {
    if is_heap(v) {
        rc_inc(v as usize);
    }
    v
}

/// Free one object — the single **free seam**. To bound worst-case latency
/// under an RTOS (a `release` of a large graph cascades into many frees), this
/// can later be made *deferred/incremental*: enqueue the object/children on a
/// pending-drop worklist and process a bounded amount per tick instead of
/// freeing recursively here. When child retain/release lands (the emitter's RC
/// wiring), release children here before `dealloc`.
fn free_object(o: usize) {
    // A container owns one reference to each child, so releasing it releases the
    // children (ownership model: insert transfers the value's ref in; free
    // releases it). This is the cascading free the RTOS note wants to bound.
    match rd(o) {
        T_LIST | T_SET | T_TUPLE => {
            for i in 0..coll_len(o) {
                p2w_release(list_get(o, i));
            }
            let d = coll_data(o);
            if d != 0 {
                dealloc(d);
            }
        }
        T_DICT => {
            for i in 0..coll_len(o) {
                p2w_release(dict_key(o, i));
                p2w_release(dict_val(o, i));
            }
            let d = coll_data(o);
            if d != 0 {
                dealloc(d);
            }
        }
        T_IARRAY | T_FARRAY => {
            // Raw scalar elements carry no refcount — just free the buffer.
            let d = coll_data(o);
            if d != 0 {
                dealloc(d);
            }
        }
        _ => {}
    }
    dealloc(o);
    live_dec();
}

/// Retain: increment a heap value's refcount (no-op for inline values).
#[unsafe(no_mangle)]
pub extern "C" fn p2w_retain(v: Value) {
    if is_heap(v) {
        rc_inc(v as usize);
    }
}

/// Release: decrement; free at zero (no-op for inline values).
#[unsafe(no_mangle)]
pub extern "C" fn p2w_release(v: Value) {
    if !is_heap(v) {
        return;
    }
    let o = v as usize;
    if rd(o + 4) > 1 {
        rc_dec(o);
    } else {
        free_object(o);
    }
}

// --- lists, dicts, iterators -----------------------------------------------
//
// Object tags. STR is `[tag][rc][len][bytes..]`. LIST and DICT share
// `[tag][rc][len][cap][data_off]` with a *separately allocated* backing buffer
// at `data_off`, so the object's offset (its Value) stays stable across growth
// (append/setindex never move the object). ITER is `[tag][rc][container][idx]`.
const T_LIST: u32 = 2;
const T_DICT: u32 = 3;
const T_ITER: u32 = 4;
/// A packed `list[int]`: same layout as a list, but elements are raw `i32` (not
/// boxed Values) and aren't refcounted. See VALUE_MODEL.md (Phase C).
const T_IARRAY: u32 = 7;
/// A packed `list[float]`: like `T_IARRAY` but elements are raw `f64` (8 bytes).
const T_FARRAY: u32 = 8;
/// A set: list-backed with dedup-on-insert (linear membership). Elements are
/// boxed Values, like a list, but unique. Equality is order-independent.
const T_SET: u32 = 9;
/// An immutable tuple: identical layout to a list, but a distinct tag — item
/// assignment is rejected, it prints with parentheses, and it isn't equal to a
/// list. The emitter builds a list and then `p2w_freeze`s it to this tag.
const T_TUPLE: u32 = 10;

fn coll_len(o: usize) -> usize {
    rd(o + 8) as usize
}
fn coll_cap(o: usize) -> usize {
    rd(o + 12) as usize
}
fn coll_data(o: usize) -> usize {
    rd(o + 16) as usize
}
fn set_len(o: usize, n: usize) {
    wr(o + 8, n as u32);
}

/// A fresh empty collection object of `tag` (no backing buffer yet).
fn coll_new(tag: u32) -> Value {
    let o = alloc(20);
    if o == 0 {
        trap("out of memory");
    }
    wr(o, tag);
    wr(o + 4, 1); // refcount
    live_inc();
    wr(o + 8, 0); // len
    wr(o + 12, 0); // cap
    wr(o + 16, 0); // data_off
    o as Value
}

/// Ensure the backing buffer holds at least `need` slots of `slot_bytes` each,
/// growing (and copying `used` slots) if necessary.
fn ensure_cap(o: usize, need: usize, used: usize, slot_bytes: usize) {
    let cap = coll_cap(o);
    if cap >= need {
        return;
    }
    let mut new_cap = if cap == 0 { 4 } else { cap * 2 };
    if new_cap < need {
        new_cap = need;
    }
    let new_data = alloc(new_cap * slot_bytes);
    if new_data == 0 {
        trap("out of memory");
    }
    let old_data = coll_data(o);
    for i in 0..(used * slot_bytes / 4) {
        wr(new_data + i * 4, rd(old_data + i * 4));
    }
    if cap != 0 {
        dealloc(old_data);
    }
    wr(o + 12, new_cap as u32);
    wr(o + 16, new_data as u32);
}

fn list_get(o: usize, i: usize) -> Value {
    rd(coll_data(o) + i * 4) as Value
}
fn list_set_at(o: usize, i: usize, x: Value) {
    wr(coll_data(o) + i * 4, x as u32);
}
fn list_push(o: usize, x: Value) {
    let len = coll_len(o);
    ensure_cap(o, len + 1, len, 4);
    wr(coll_data(o) + len * 4, x as u32);
    set_len(o, len + 1);
}

fn dict_key(o: usize, i: usize) -> Value {
    rd(coll_data(o) + i * 8) as Value
}
fn dict_val(o: usize, i: usize) -> Value {
    rd(coll_data(o) + i * 8 + 4) as Value
}
fn dict_find(o: usize, key: Value) -> Option<usize> {
    (0..coll_len(o)).find(|&i| value_eq(dict_key(o, i), key))
}
fn dict_set(o: usize, key: Value, val: Value) {
    // Ownership: the new key/value arrive owned (transferred in). On update we
    // keep the existing key, so release the old value and the redundant new key.
    if let Some(i) = dict_find(o, key) {
        p2w_release(dict_val(o, i));
        wr(coll_data(o) + i * 8 + 4, val as u32);
        p2w_release(key);
        return;
    }
    let len = coll_len(o);
    ensure_cap(o, len + 1, len, 8); // 8 bytes/pair
    wr(coll_data(o) + len * 8, key as u32);
    wr(coll_data(o) + len * 8 + 4, val as u32);
    set_len(o, len + 1);
}

/// Normalize an index (negative-from-end) and bounds-check, or trap.
fn norm_index(index: Value, n: i64) -> i64 {
    let i = match num(index) {
        Some(i) => i,
        None => trap("indices must be integers"),
    };
    let r = if i < 0 { i + n } else { i };
    if r < 0 || r >= n {
        trap("index out of range");
    }
    r
}

/// `len(v)` for any collection (string/list/dict all store len at +8).
#[unsafe(no_mangle)]
pub extern "C" fn p2w_len(v: Value) -> Value {
    if is_heap(v)
        && matches!(
            obj_tag(v),
            T_STR | T_LIST | T_DICT | T_IARRAY | T_FARRAY | T_SET | T_TUPLE
        )
    {
        return make_int(coll_len(v as usize) as i64);
    }
    trap("object has no len()")
}

/// `target[index]` — string char, list element, or dict value.
#[unsafe(no_mangle)]
pub extern "C" fn p2w_index(target: Value, index: Value) -> Value {
    if !is_heap(target) {
        trap("object is not subscriptable");
    }
    let o = target as usize;
    match obj_tag(target) {
        T_STR => {
            let i = norm_index(index, coll_len(o) as i64);
            str_alloc(&[str_byte(target, i as usize)])
        }
        T_LIST | T_TUPLE => {
            let i = norm_index(index, coll_len(o) as i64);
            owned(list_get(o, i as usize)) // hand the caller an owned ref
        }
        T_DICT => match dict_find(o, index) {
            Some(i) => owned(dict_val(o, i)),
            None => trap("key not found"),
        },
        _ => trap("object is not subscriptable"),
    }
}

/// `target[index] = value` — list element or dict entry.
#[unsafe(no_mangle)]
pub extern "C" fn p2w_setindex(target: Value, index: Value, value: Value) {
    if !is_heap(target) {
        trap("object does not support item assignment");
    }
    let o = target as usize;
    match obj_tag(target) {
        T_LIST => {
            let i = norm_index(index, coll_len(o) as i64);
            p2w_release(list_get(o, i as usize)); // release the replaced element
            list_set_at(o, i as usize, value); // new value transferred in
        }
        T_DICT => dict_set(o, index, value),
        T_TUPLE => trap("a tuple is immutable — you can't change its items"),
        _ => trap("object does not support item assignment"),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_list_new() -> Value {
    coll_new(T_LIST)
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_list_append(list: Value, v: Value) -> Value {
    if !(is_heap(list) && obj_tag(list) == T_LIST) {
        trap("append() expects a list");
    }
    list_push(list as usize, v);
    V_NONE
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_dict_new() -> Value {
    coll_new(T_DICT)
}

/// Turn a freshly-built list into an (immutable) tuple by re-tagging it in place
/// — same layout, so no copy. The emitter builds a tuple literal as a list then
/// freezes it. A no-op on a non-list (defensive).
#[unsafe(no_mangle)]
pub extern "C" fn p2w_freeze(list: Value) -> Value {
    if is_heap(list) && obj_tag(list) == T_LIST {
        wr(list as usize, T_TUPLE);
    }
    list
}

// --- packed int arrays (list[int]) -----------------------------------------
// A T_IARRAY stores raw i32 elements (no per-element refcount), so a list of
// ints costs one heap object + a flat i32 buffer instead of boxed elements. The
// element ABI is raw `i32`, not `Value`. See VALUE_MODEL.md (Phase C).

#[unsafe(no_mangle)]
pub extern "C" fn p2w_iarray_new() -> Value {
    coll_new(T_IARRAY)
}

/// Append a raw int element.
#[unsafe(no_mangle)]
pub extern "C" fn p2w_iarray_push(arr: Value, x: i32) {
    if !(is_heap(arr) && obj_tag(arr) == T_IARRAY) {
        trap("expected an int array");
    }
    list_push(arr as usize, x as Value);
}

/// Bounds-checked read of a raw int element (supports Python negative indices).
#[unsafe(no_mangle)]
pub extern "C" fn p2w_iarray_get(arr: Value, idx: i32) -> i32 {
    let o = arr as usize;
    let i = checked_index(o, idx, T_IARRAY);
    list_get(o, i) as i32
}

/// Bounds-checked write of a raw int element.
#[unsafe(no_mangle)]
pub extern "C" fn p2w_iarray_set(arr: Value, idx: i32, x: i32) {
    let o = arr as usize;
    let i = checked_index(o, idx, T_IARRAY);
    list_set_at(o, i, x as Value);
}

/// Normalize + bounds-check a raw index against a packed array of the given tag
/// (traps on a wrong type or out-of-range index).
fn checked_index(o: usize, idx: i32, tag: u32) -> usize {
    if !(is_heap(o as Value) && obj_tag(o as Value) == tag) {
        trap("expected an array");
    }
    let n = coll_len(o) as i64;
    let i = if (idx as i64) < 0 {
        idx as i64 + n
    } else {
        idx as i64
    };
    if i < 0 || i >= n {
        trap("index out of range");
    }
    i as usize
}

// --- packed float arrays (list[float]) -------------------------------------
// Like T_IARRAY but each element is a raw f64 (8-byte slots).

fn farray_get(o: usize, i: usize) -> f64 {
    unsafe { core::ptr::read_unaligned(heap_base().add(coll_data(o) + i * 8) as *const f64) }
}
fn farray_set_at(o: usize, i: usize, x: f64) {
    unsafe { core::ptr::write_unaligned(heap_base().add(coll_data(o) + i * 8) as *mut f64, x) };
}
fn farray_push(o: usize, x: f64) {
    let len = coll_len(o);
    ensure_cap(o, len + 1, len, 8);
    farray_set_at(o, len, x);
    set_len(o, len + 1);
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_farray_new() -> Value {
    coll_new(T_FARRAY)
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_farray_push(arr: Value, x: f64) {
    if !(is_heap(arr) && obj_tag(arr) == T_FARRAY) {
        trap("expected a float array");
    }
    farray_push(arr as usize, x);
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_farray_get(arr: Value, idx: i32) -> f64 {
    let o = arr as usize;
    let i = checked_index(o, idx, T_FARRAY);
    farray_get(o, i)
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_farray_set(arr: Value, idx: i32, x: f64) {
    let o = arr as usize;
    let i = checked_index(o, idx, T_FARRAY);
    farray_set_at(o, i, x);
}

// --- sets ------------------------------------------------------------------
// A set reuses the list storage but keeps elements unique. Membership and the
// set operators (& | ^ -) are linear scans — fine for the small sets a teaching
// program builds. Set equality (in value_eq) is order-independent.

/// Whether the set/list at offset `o` already contains an element equal to `v`.
fn coll_contains(o: usize, v: Value) -> bool {
    (0..coll_len(o)).any(|i| value_eq(list_get(o, i), v))
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_set_new() -> Value {
    coll_new(T_SET)
}

/// `set(iterable)` — a new set of the iterable's (deduplicated) elements.
/// Borrows the iterable; the set takes its own (retained) refs.
#[unsafe(no_mangle)]
pub extern "C" fn p2w_set_of(iterable: Value) -> Value {
    if !(is_heap(iterable)
        && matches!(obj_tag(iterable), T_STR | T_LIST | T_DICT | T_SET | T_TUPLE))
    {
        trap("set() needs an iterable");
    }
    let r = p2w_set_new();
    for i in 0..container_len(iterable) {
        // element_at returns an owned element; set_add transfers it (or releases
        // it as a duplicate).
        p2w_set_add(r, element_at(iterable, i));
    }
    r
}

/// Add `v` (transferred); a duplicate is dropped (its ref released).
#[unsafe(no_mangle)]
pub extern "C" fn p2w_set_add(set: Value, v: Value) {
    if !(is_heap(set) && obj_tag(set) == T_SET) {
        trap("expected a set");
    }
    // Set members must be immutable (hashable). A list/dict/set can change, so it
    // can't be a member — use a tuple. (A tuple is allowed.)
    if is_heap(v) && matches!(obj_tag(v), T_LIST | T_DICT | T_SET) {
        trap("a set can't contain a list, dict, or set — use a tuple");
    }
    if coll_contains(set as usize, v) {
        p2w_release(v); // redundant — the set already owns an equal element
    } else {
        list_push(set as usize, v);
    }
}

/// `value in container` — membership for sets/lists (by equality), dict keys, and
/// substrings. Returns a boxed bool.
#[unsafe(no_mangle)]
pub extern "C" fn p2w_in(value: Value, container: Value) -> Value {
    if !is_heap(container) {
        trap("argument to `in` is not a container");
    }
    let o = container as usize;
    let found = match obj_tag(container) {
        T_SET | T_LIST | T_TUPLE => coll_contains(o, value),
        T_DICT => dict_find(o, value).is_some(),
        T_STR => str_contains(container, value),
        _ => trap("argument to `in` is not a container"),
    };
    make_bool(found)
}

/// `value not in container`.
#[unsafe(no_mangle)]
pub extern "C" fn p2w_notin(value: Value, container: Value) -> Value {
    make_bool(!p2w_truthy(p2w_in(value, container)))
}

/// New set of the elements common to both (intersection, `a & b`).
fn set_intersect(a: usize, b: usize) -> Value {
    let r = coll_new(T_SET);
    for i in 0..coll_len(a) {
        let e = list_get(a, i);
        if coll_contains(b, e) {
            list_push(r as usize, owned(e));
        }
    }
    r
}

/// New set of all elements from either (union, `a | b`).
fn set_union(a: usize, b: usize) -> Value {
    let r = coll_new(T_SET);
    for i in 0..coll_len(a) {
        list_push(r as usize, owned(list_get(a, i)));
    }
    for i in 0..coll_len(b) {
        let e = list_get(b, i);
        if !coll_contains(r as usize, e) {
            list_push(r as usize, owned(e));
        }
    }
    r
}

/// New set of elements in exactly one (symmetric difference, `a ^ b`).
fn set_symdiff(a: usize, b: usize) -> Value {
    let r = coll_new(T_SET);
    for i in 0..coll_len(a) {
        let e = list_get(a, i);
        if !coll_contains(b, e) {
            list_push(r as usize, owned(e));
        }
    }
    for i in 0..coll_len(b) {
        let e = list_get(b, i);
        if !coll_contains(a, e) {
            list_push(r as usize, owned(e));
        }
    }
    r
}

/// New set of elements in `a` but not `b` (difference, `a - b`).
fn set_difference(a: usize, b: usize) -> Value {
    let r = coll_new(T_SET);
    for i in 0..coll_len(a) {
        let e = list_get(a, i);
        if !coll_contains(b, e) {
            list_push(r as usize, owned(e));
        }
    }
    r
}

fn both_sets(a: Value, b: Value) -> bool {
    is_heap(a) && obj_tag(a) == T_SET && is_heap(b) && obj_tag(b) == T_SET
}

/// Remove the element equal to `v` from the set at `o` (releasing it and shifting
/// the tail down). Returns whether it was present. Does not touch `v` itself.
fn set_remove(o: usize, v: Value) -> bool {
    let n = coll_len(o);
    for i in 0..n {
        if value_eq(list_get(o, i), v) {
            p2w_release(list_get(o, i)); // the set owned this element
            for j in i..(n - 1) {
                list_set_at(o, j, list_get(o, j + 1));
            }
            set_len(o, n - 1);
            return true;
        }
    }
    false
}

/// Whether every element of set `a` is in set `b` (a ⊆ b).
fn set_subset(a: usize, b: usize) -> bool {
    (0..coll_len(a)).all(|i| coll_contains(b, list_get(a, i)))
}

/// Whether the string `needle` occurs in the string `hay` (naive search).
fn str_contains(hay: Value, needle: Value) -> bool {
    if !(is_heap(needle) && obj_tag(needle) == T_STR) {
        trap("the left operand of `in` on a string must be a string");
    }
    let (hl, nl) = (str_len(hay), str_len(needle));
    if nl == 0 {
        return true;
    }
    if nl > hl {
        return false;
    }
    (0..=(hl - nl)).any(|start| (0..nl).all(|j| str_byte(hay, start + j) == str_byte(needle, j)))
}

/// `a & b` — set intersection, or integer bitwise-and.
#[unsafe(no_mangle)]
pub extern "C" fn p2w_band(a: Value, b: Value) -> Value {
    if both_sets(a, b) {
        return set_intersect(a as usize, b as usize);
    }
    match (num(a), num(b)) {
        (Some(x), Some(y)) => make_int(x & y),
        _ => trap("& expects two ints or two sets"),
    }
}

/// `a | b` — set union, or integer bitwise-or.
#[unsafe(no_mangle)]
pub extern "C" fn p2w_bor(a: Value, b: Value) -> Value {
    if both_sets(a, b) {
        return set_union(a as usize, b as usize);
    }
    match (num(a), num(b)) {
        (Some(x), Some(y)) => make_int(x | y),
        _ => trap("| expects two ints or two sets"),
    }
}

/// `a ^ b` — set symmetric difference, or integer bitwise-xor.
#[unsafe(no_mangle)]
pub extern "C" fn p2w_bxor(a: Value, b: Value) -> Value {
    if both_sets(a, b) {
        return set_symdiff(a as usize, b as usize);
    }
    match (num(a), num(b)) {
        (Some(x), Some(y)) => make_int(x ^ y),
        _ => trap("^ expects two ints or two sets"),
    }
}

// --- iteration protocol ----------------------------------------------------

/// Number of elements a for-each yields over `c`.
fn container_len(c: Value) -> usize {
    coll_len(c as usize)
}

/// The element at position `i` of a for-each over `c` (list element, string
/// char, dict key, or set member).
fn element_at(c: Value, i: usize) -> Value {
    match obj_tag(c) {
        T_STR => str_alloc(&[str_byte(c, i)]), // freshly allocated -> already owned
        T_LIST | T_SET | T_TUPLE => owned(list_get(c as usize, i)),
        T_DICT => owned(dict_key(c as usize, i)),
        _ => trap("object is not iterable"),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_iter(c: Value) -> Value {
    if !(is_heap(c) && matches!(obj_tag(c), T_STR | T_LIST | T_DICT | T_SET | T_TUPLE)) {
        trap("object is not iterable");
    }
    let o = alloc(16);
    if o == 0 {
        trap("out of memory");
    }
    wr(o, T_ITER);
    wr(o + 4, 1);
    live_inc();
    wr(o + 8, c as u32); // container
    wr(o + 12, 0); // index
    o as Value
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_iter_has(it: Value) -> bool {
    let o = it as usize;
    let c = rd(o + 8) as Value;
    (rd(o + 12) as usize) < container_len(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_iter_next(it: Value) -> Value {
    let o = it as usize;
    let c = rd(o + 8) as Value;
    let i = rd(o + 12) as usize;
    wr(o + 12, (i + 1) as u32);
    element_at(c, i)
}

// --- method dispatch (by name, on the receiver's type) ---------------------

/// True if the (ptr,len) method name equals `expected`.
///
/// # Safety
/// `ptr` must point to `len` valid bytes.
unsafe fn name_eq(ptr: *const u8, len: i32, expected: &[u8]) -> bool {
    len as usize == expected.len()
        && unsafe { core::slice::from_raw_parts(ptr, len as usize) } == expected
}

/// `recv.method()` — 0-argument methods (e.g. list `pop`).
///
/// # Safety
/// `name` must point to `name_len` valid bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn p2w_method0(recv: Value, name: *const u8, name_len: i32) -> Value {
    if is_heap(recv) && obj_tag(recv) == T_LIST && unsafe { name_eq(name, name_len, b"pop") } {
        let o = recv as usize;
        let len = coll_len(o);
        if len == 0 {
            trap("pop from empty list");
        }
        let v = list_get(o, len - 1);
        set_len(o, len - 1);
        return v;
    }
    if is_heap(recv) && obj_tag(recv) == T_SET {
        let o = recv as usize;
        if unsafe { name_eq(name, name_len, b"clear") } {
            for i in 0..coll_len(o) {
                p2w_release(list_get(o, i));
            }
            set_len(o, 0);
            return V_NONE;
        }
        if unsafe { name_eq(name, name_len, b"copy") } {
            return p2w_set_of(recv); // fresh set, elements retained
        }
        if unsafe { name_eq(name, name_len, b"pop") } {
            let len = coll_len(o);
            if len == 0 {
                trap("pop from an empty set");
            }
            let v = list_get(o, len - 1); // arbitrary element, transferred out
            set_len(o, len - 1);
            return v;
        }
    }
    trap("method not supported in the native backend yet")
}

/// `recv.method(a)` — 1-argument methods (e.g. list `append`, `pop(i)`).
///
/// # Safety
/// `name` must point to `name_len` valid bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn p2w_method1(
    recv: Value,
    name: *const u8,
    name_len: i32,
    a: Value,
) -> Value {
    if is_heap(recv) && obj_tag(recv) == T_LIST {
        if unsafe { name_eq(name, name_len, b"append") } {
            list_push(recv as usize, a);
            return V_NONE;
        }
        if unsafe { name_eq(name, name_len, b"pop") } {
            let o = recv as usize;
            let n = coll_len(o) as i64;
            let i = norm_index(a, n) as usize;
            let v = list_get(o, i);
            // shift the tail down by one
            for j in i..(coll_len(o) - 1) {
                list_set_at(o, j, list_get(o, j + 1));
            }
            set_len(o, coll_len(o) - 1);
            return v;
        }
    }
    // Set methods. The argument `a` is transferred to us; `add` stores it (or
    // releases a duplicate), the rest read it and release it before returning.
    if is_heap(recv) && obj_tag(recv) == T_SET {
        let o = recv as usize;
        if unsafe { name_eq(name, name_len, b"add") } {
            p2w_set_add(recv, a);
            return V_NONE;
        }
        if unsafe { name_eq(name, name_len, b"discard") } {
            set_remove(o, a);
            p2w_release(a);
            return V_NONE;
        }
        if unsafe { name_eq(name, name_len, b"remove") } {
            let found = set_remove(o, a);
            p2w_release(a);
            if !found {
                trap("set.remove(x): x not in set");
            }
            return V_NONE;
        }
        // other-set operations: new set, then release the argument.
        let op: Option<fn(usize, usize) -> Value> = if unsafe { name_eq(name, name_len, b"union") }
        {
            Some(set_union)
        } else if unsafe { name_eq(name, name_len, b"intersection") } {
            Some(set_intersect)
        } else if unsafe { name_eq(name, name_len, b"difference") } {
            Some(set_difference)
        } else if unsafe { name_eq(name, name_len, b"symmetric_difference") } {
            Some(set_symdiff)
        } else {
            None
        };
        if let Some(f) = op {
            if !(is_heap(a) && obj_tag(a) == T_SET) {
                trap("set method expects a set argument");
            }
            let r = f(o, a as usize);
            p2w_release(a);
            return r;
        }
        if unsafe { name_eq(name, name_len, b"issubset") } {
            let r = make_bool(is_heap(a) && obj_tag(a) == T_SET && set_subset(o, a as usize));
            p2w_release(a);
            return r;
        }
        if unsafe { name_eq(name, name_len, b"issuperset") } {
            let r = make_bool(is_heap(a) && obj_tag(a) == T_SET && set_subset(a as usize, o));
            p2w_release(a);
            return r;
        }
    }
    trap("method not supported in the native backend yet")
}

/// `recv.method(a, b)` — 2-argument methods (none yet; reserved).
///
/// # Safety
/// `name` must point to `name_len` valid bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn p2w_method2(
    _recv: Value,
    _name: *const u8,
    _name_len: i32,
    _a: Value,
    _b: Value,
) -> Value {
    trap("method not supported in the native backend yet")
}

// --- printing --------------------------------------------------------------

/// Write a value's `str()` form via the byte sink `out` (no trailing newline).
/// Generic so the formatting logic is testable without the device's putc.
fn write_value(v: Value, out: &mut impl FnMut(u8)) {
    if is_int(v) {
        write_int(as_int(v), out);
        return;
    } else if is_float(v) {
        write_float(as_f64(v), out);
        return;
    } else if v == V_TRUE {
        return write_bytes(b"True", out);
    } else if v == V_FALSE {
        return write_bytes(b"False", out);
    } else if v == V_NONE {
        return write_bytes(b"None", out);
    }
    if !is_heap(v) {
        return write_bytes(b"<object>", out);
    }
    let o = v as usize;
    match obj_tag(v) {
        T_STR => {
            // print() shows a string bare (no quotes), like Python str().
            for i in 0..str_len(v) {
                out(str_byte(v, i));
            }
        }
        T_LIST => {
            out(b'[');
            for i in 0..coll_len(o) {
                if i > 0 {
                    write_bytes(b", ", out);
                }
                write_repr(list_get(o, i), out);
            }
            out(b']');
        }
        T_TUPLE => {
            let n = coll_len(o);
            out(b'(');
            for i in 0..n {
                if i > 0 {
                    write_bytes(b", ", out);
                }
                write_repr(list_get(o, i), out);
            }
            if n == 1 {
                out(b','); // a 1-tuple keeps its trailing comma: (5,)
            }
            out(b')');
        }
        T_DICT => {
            out(b'{');
            for i in 0..coll_len(o) {
                if i > 0 {
                    write_bytes(b", ", out);
                }
                write_repr(dict_key(o, i), out);
                write_bytes(b": ", out);
                write_repr(dict_val(o, i), out);
            }
            out(b'}');
        }
        T_SET => {
            out(b'{');
            for i in 0..coll_len(o) {
                if i > 0 {
                    write_bytes(b", ", out);
                }
                write_repr(list_get(o, i), out);
            }
            out(b'}');
        }
        T_IARRAY => {
            // A packed int array prints like a list; elements are raw ints.
            out(b'[');
            for i in 0..coll_len(o) {
                if i > 0 {
                    write_bytes(b", ", out);
                }
                write_int(list_get(o, i) as i32 as i64, out);
            }
            out(b']');
        }
        T_FARRAY => {
            out(b'[');
            for i in 0..coll_len(o) {
                if i > 0 {
                    write_bytes(b", ", out);
                }
                write_float(farray_get(o, i), out);
            }
            out(b']');
        }
        _ => write_bytes(b"<object>", out),
    }
}

/// Like `write_value`, but strings are quoted (the `repr()` form used *inside*
/// containers, matching Python).
fn write_repr(v: Value, out: &mut impl FnMut(u8)) {
    if is_heap(v) && obj_tag(v) == T_STR {
        out(b'\'');
        for i in 0..str_len(v) {
            out(str_byte(v, i));
        }
        out(b'\'');
    } else {
        write_value(v, out);
    }
}

fn write_bytes(b: &[u8], out: &mut impl FnMut(u8)) {
    for &c in b {
        out(c);
    }
}

/// Decimal formatting of an integer with no allocation (`no_std`-friendly).
fn write_int(n: i64, out: &mut impl FnMut(u8)) {
    if n < 0 {
        out(b'-');
    }
    // Work in the non-positive domain so i64::MIN doesn't overflow on negate.
    let mut neg = if n < 0 { n } else { n.wrapping_neg() };
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (-(neg % 10)) as u8;
        neg /= 10;
        if neg == 0 {
            break;
        }
    }
    write_bytes(&buf[i..], out);
}

/// Decimal of an unsigned integer (used for a float's integer part).
fn write_u64(mut n: u64, out: &mut impl FnMut(u8)) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    write_bytes(&buf[i..], out);
}

/// Format an `f64` Python-style (always a decimal point: `2.0`, `3.5`, `0.1`).
///
/// This is a fixed-point formatter good for the teaching range: it rounds to 15
/// fractional digits and trims trailing zeros. It is **not** CPython's
/// shortest-round-trip `repr` and does **not** use scientific notation, so very
/// large/small magnitudes print in long fixed-point — a documented v1 limit.
fn write_float(x: f64, out: &mut impl FnMut(u8)) {
    if x.is_nan() {
        return write_bytes(b"nan", out);
    }
    let neg = x.is_sign_negative(); // also catches -0.0, like Python
    let mut ax = if neg { -x } else { x };
    if neg {
        out(b'-');
    }
    if ax.is_infinite() {
        return write_bytes(b"inf", out);
    }
    const SCALE: u64 = 1_000_000_000_000_000; // 10^15
    let mut ip = libm::trunc(ax);
    ax -= ip;
    let mut frac = libm::round(ax * SCALE as f64) as u64;
    if frac >= SCALE {
        // rounding carried into the integer part (e.g. 0.9999… -> 1.0)
        ip += 1.0;
        frac -= SCALE;
    }
    write_u64(ip as u64, out);
    out(b'.');
    // 15-digit zero-padded fraction, then trim trailing zeros (keep one).
    let mut buf = [b'0'; 15];
    let mut f = frac;
    let mut i = buf.len();
    while i > 0 {
        i -= 1;
        buf[i] = b'0' + (f % 10) as u8;
        f /= 10;
    }
    let mut end = buf.len();
    while end > 1 && buf[end - 1] == b'0' {
        end -= 1;
    }
    write_bytes(&buf[..end], out);
}

unsafe extern "C" {
    /// The platform byte sink: USB-CDC on the device. (Provided at final link;
    /// host tests supply a stub — see the tests module.)
    fn p2w_putc(c: u8);
}

#[unsafe(no_mangle)]
pub extern "C" fn p2w_print(v: Value) {
    write_value(v, &mut |c| unsafe { p2w_putc(c) });
    unsafe { p2w_putc(b'\n') };
}

/// Halt on an unrecoverable runtime error. On the device this will report over
/// USB-CDC and stop; for now it panics (host) / loops (device).
fn trap(_msg: &str) -> ! {
    #[cfg(test)]
    panic!("p2w runtime trap: {_msg}");
    #[cfg(not(test))]
    loop {
        core::hint::spin_loop();
    }
}

/// Panic handler for `no_std` artifacts — the host run-oracle's static lib and
/// the eventual bare-metal device build. (Under `cfg(test)` the crate is `std`,
/// which supplies its own handler, so this is excluded there.) Anything that
/// panics here — a Rust overflow check, say — is an unrecoverable bug, so we
/// halt like `trap`.
#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Satisfy the linker for the test binary (p2w_print references p2w_putc).
    #[unsafe(no_mangle)]
    extern "C" fn p2w_putc(_c: u8) {}

    // The heap is one shared `static mut`, but `cargo test` runs tests in
    // parallel — so heap-touching tests serialize on this lock (and reset the
    // arena under it). Tests using only inline ints/bools/None don't need it.
    static HEAP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn heap_guard() -> std::sync::MutexGuard<'static, ()> {
        let g = HEAP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        heap_reset();
        g
    }

    fn str_val(s: &str) -> Value {
        str_alloc(s.as_bytes())
    }

    /// Drive a for-each iterator to completion, collecting integer elements.
    fn collect_ints(c: Value) -> Vec<i64> {
        let it = p2w_iter(c);
        let mut out = Vec::new();
        while p2w_iter_has(it) {
            out.push(as_int(p2w_iter_next(it)));
        }
        out
    }

    fn shown(v: Value) -> String {
        let mut s = Vec::new();
        write_value(v, &mut |c| s.push(c));
        String::from_utf8(s).unwrap()
    }

    #[test]
    fn ints_round_trip_and_print() {
        assert_eq!(as_int(p2w_int(42)), 42);
        assert_eq!(as_int(p2w_int(-7)), -7);
        assert_eq!(shown(p2w_int(0)), "0");
        assert_eq!(shown(p2w_int(12345)), "12345");
        assert_eq!(shown(p2w_int(-9876)), "-9876");
    }

    #[test]
    fn arithmetic_matches_python_for_ints() {
        assert_eq!(as_int(p2w_add(p2w_int(2), p2w_int(3))), 5);
        assert_eq!(as_int(p2w_mul(p2w_int(6), p2w_int(7))), 42);
        assert_eq!(as_int(p2w_sub(p2w_int(1), p2w_int(4))), -3);
        // floor division and modulo follow the divisor's sign (Python).
        assert_eq!(as_int(p2w_floordiv(p2w_int(7), p2w_int(2))), 3);
        assert_eq!(as_int(p2w_floordiv(p2w_int(-7), p2w_int(2))), -4);
        assert_eq!(as_int(p2w_mod(p2w_int(-7), p2w_int(3))), 2);
        assert_eq!(as_int(p2w_neg(p2w_int(5))), -5);
    }

    #[test]
    fn bools_none_and_truthiness() {
        assert_eq!(shown(p2w_bool(1)), "True");
        assert_eq!(shown(p2w_bool(0)), "False");
        assert_eq!(shown(p2w_none()), "None");
        assert!(p2w_truthy(p2w_int(3)));
        assert!(!p2w_truthy(p2w_int(0)));
        assert!(p2w_truthy(p2w_bool(1)));
        assert!(!p2w_truthy(p2w_none()));
        // bool acts as int in arithmetic (True + True == 2).
        assert_eq!(as_int(p2w_add(p2w_bool(1), p2w_bool(1))), 2);
    }

    #[test]
    fn comparisons_and_equality() {
        assert_eq!(p2w_lt(p2w_int(1), p2w_int(2)), V_TRUE);
        assert_eq!(p2w_ge(p2w_int(2), p2w_int(2)), V_TRUE);
        assert_eq!(p2w_eq(p2w_int(5), p2w_int(5)), V_TRUE);
        assert_eq!(p2w_ne(p2w_int(5), p2w_int(6)), V_TRUE);
        assert_eq!(p2w_eq(p2w_bool(1), p2w_int(1)), V_TRUE); // True == 1
        assert_eq!(p2w_eq(p2w_none(), p2w_none()), V_TRUE);
        assert_eq!(p2w_eq(p2w_none(), p2w_int(0)), V_FALSE);
        assert_eq!(p2w_not(p2w_int(0)), V_TRUE);
    }

    #[test]
    fn floats_format_like_python() {
        let _g = heap_guard();
        assert_eq!(shown(p2w_float(2.0)), "2.0"); // always a decimal point
        assert_eq!(shown(p2w_float(3.5)), "3.5");
        assert_eq!(shown(p2w_float(0.5)), "0.5");
        assert_eq!(shown(p2w_float(0.1)), "0.1"); // trims float noise
        assert_eq!(shown(p2w_float(-2.75)), "-2.75");
        assert_eq!(shown(p2w_float(123.0)), "123.0");
        assert_eq!(shown(p2w_float(0.9999999999999999)), "1.0"); // rounds & carries
    }

    #[test]
    fn float_arithmetic_and_promotion() {
        let _g = heap_guard();
        // any float operand promotes the result to float
        assert_eq!(shown(p2w_add(p2w_float(1.5), p2w_int(2))), "3.5");
        assert_eq!(shown(p2w_mul(p2w_int(2), p2w_float(2.5))), "5.0");
        assert_eq!(shown(p2w_sub(p2w_float(5.0), p2w_float(1.25))), "3.75");
        assert_eq!(shown(p2w_neg(p2w_float(4.0))), "-4.0");
        // int-only arithmetic stays int
        assert_eq!(shown(p2w_add(p2w_int(2), p2w_int(3))), "5");
    }

    #[test]
    fn true_division_is_always_float() {
        let _g = heap_guard();
        assert_eq!(shown(p2w_div(p2w_int(7), p2w_int(2))), "3.5");
        assert_eq!(shown(p2w_div(p2w_int(4), p2w_int(2))), "2.0"); // exact -> still float
        assert_eq!(shown(p2w_div(p2w_float(1.0), p2w_int(4))), "0.25");
    }

    #[test]
    fn power_keeps_int_when_it_can() {
        let _g = heap_guard();
        assert_eq!(shown(p2w_pow(p2w_int(2), p2w_int(10))), "1024"); // int ** +int = int
        assert_eq!(shown(p2w_pow(p2w_int(2), p2w_int(0))), "1");
        assert_eq!(shown(p2w_pow(p2w_int(2), p2w_int(-1))), "0.5"); // negative exp -> float
        assert_eq!(shown(p2w_pow(p2w_float(2.0), p2w_int(3))), "8.0"); // float base -> float
    }

    #[test]
    fn float_floordiv_mod_compare_eq() {
        let _g = heap_guard();
        assert_eq!(shown(p2w_floordiv(p2w_float(7.0), p2w_int(2))), "3.0");
        assert_eq!(shown(p2w_mod(p2w_float(7.5), p2w_int(2))), "1.5");
        assert_eq!(p2w_lt(p2w_float(1.5), p2w_int(2)), V_TRUE);
        assert_eq!(p2w_ge(p2w_float(2.0), p2w_int(2)), V_TRUE);
        assert_eq!(p2w_eq(p2w_int(1), p2w_float(1.0)), V_TRUE); // cross-type ==
        assert!(p2w_truthy(p2w_float(0.1)));
        assert!(!p2w_truthy(p2w_float(0.0)));
    }

    #[test]
    fn packed_int_array_stores_raw_elements_and_frees_cleanly() {
        let _g = heap_guard();
        let arr = p2w_iarray_new();
        assert_eq!(p2w_live(), 1); // one heap object (the array)
        p2w_iarray_push(arr, 10);
        p2w_iarray_push(arr, 20);
        p2w_iarray_push(arr, -30);
        assert_eq!(as_int(p2w_len(arr)), 3);
        assert_eq!(p2w_iarray_get(arr, 0), 10);
        assert_eq!(p2w_iarray_get(arr, 2), -30);
        assert_eq!(p2w_iarray_get(arr, -1), -30); // Python negative index
        p2w_iarray_set(arr, 1, 99);
        assert_eq!(p2w_iarray_get(arr, 1), 99);
        assert_eq!(shown(arr), "[10, 99, -30]");
        // Raw elements carry no refcount; release frees the object + buffer only.
        p2w_release(arr);
        assert_eq!(p2w_live(), 0);
    }

    #[test]
    fn unique_reflects_refcount() {
        let _g = heap_guard();
        let a = str_val("hi");
        assert!(p2w_unique(a)); // rc == 1
        rc_inc(a as usize); // share it
        assert!(!p2w_unique(a)); // rc == 2
        rc_dec(a as usize);
        assert!(p2w_unique(a));
        assert!(!p2w_unique(make_int(5))); // inline values are never "unique"
        p2w_release(a);
    }

    #[test]
    fn packed_float_array_stores_raw_doubles() {
        let _g = heap_guard();
        let arr = p2w_farray_new();
        p2w_farray_push(arr, 1.5);
        p2w_farray_push(arr, 2.0);
        p2w_farray_push(arr, -0.25);
        assert_eq!(as_int(p2w_len(arr)), 3);
        assert_eq!(p2w_farray_get(arr, 0), 1.5);
        assert_eq!(p2w_farray_get(arr, -1), -0.25); // negative index
        p2w_farray_set(arr, 1, 9.5);
        assert_eq!(p2w_farray_get(arr, 1), 9.5);
        assert_eq!(shown(arr), "[1.5, 9.5, -0.25]");
        p2w_release(arr);
        assert_eq!(p2w_live(), 0);
    }

    #[test]
    fn large_ints_box_on_the_heap_and_round_trip() {
        let _g = heap_guard();
        // Inside the inline range: stays an immediate, no heap object.
        let small = make_int(1000);
        assert!(is_int(small) && !is_heap(small));
        assert_eq!(as_int(small), 1000);
        // Outside ±2^29: heap-boxed full i32, no truncation (the old bug).
        let big = make_int(2_000_000_000);
        assert!(
            is_int(big) && is_heap(big),
            "large int should be heap-boxed"
        );
        assert_eq!(as_int(big), 2_000_000_000);
        let neg = make_int(-2_000_000_000);
        assert!(is_heap(neg));
        assert_eq!(as_int(neg), -2_000_000_000);
        // Arithmetic round-trips through the heap box.
        let sum = p2w_add(big, make_int(7));
        assert_eq!(as_int(sum), 2_000_000_007);
        // Heap ints are real refcounted objects; releasing frees them.
        p2w_release(big);
        p2w_release(neg);
        p2w_release(sum);
        assert_eq!(p2w_live(), 0);
    }

    #[test]
    fn live_count_tracks_births_and_frees() {
        let _g = heap_guard(); // resets the arena AND LIVE
        assert_eq!(p2w_live(), 0);
        let s = str_val("hello");
        assert_eq!(p2w_live(), 1);
        let xs = p2w_list_new();
        assert_eq!(p2w_live(), 2);
        p2w_list_append(xs, s); // transfers s into the list
        p2w_release(xs); // frees the list AND its string child
        assert_eq!(
            p2w_live(),
            0,
            "releasing a container must free its children"
        );
    }

    #[test]
    fn can_reuse_checks_tag_uniqueness_and_length() {
        let _g = heap_guard();
        let xs = p2w_list_new();
        p2w_list_append(xs, p2w_int(1));
        p2w_list_append(xs, p2w_int(2));
        assert!(p2w_can_reuse_list(xs, 2));
        assert!(!p2w_can_reuse_list(xs, 3), "length must match exactly");
        assert!(
            !p2w_can_reuse_iarray(xs, 2),
            "tag must match (list != iarray)"
        );
        // A string is heap + unique but the wrong tag — the Boxed-slot hazard.
        let s = str_val("ab");
        assert!(!p2w_can_reuse_list(s, 2));
        // Aliased (rc 2) — never reusable.
        p2w_retain(xs);
        assert!(!p2w_can_reuse_list(xs, 2));
        p2w_release(xs);
        assert!(p2w_can_reuse_list(xs, 2), "back to unique");
        // Inline values are never reusable.
        assert!(!p2w_can_reuse_list(p2w_int(7), 1));
        p2w_release(s);
        p2w_release(xs);
    }

    #[test]
    fn add_assign_grows_strings_in_place_and_falls_back() {
        let _g = heap_guard();
        // In place: "ab"'s block has alignment spare — a 2-byte append fits.
        let s = str_val("ab");
        let suffix = str_val("cd");
        let live_before = p2w_live();
        let r = p2w_add_assign(s, suffix);
        assert_eq!(r, s, "unique append with capacity reuses the same block");
        assert_eq!(shown(r), "abcd");
        assert_eq!(p2w_live(), live_before, "no new object");
        // Grow: a big suffix forces a slack realloc; old is consumed.
        let big = str_val("efghijklmnop");
        let live_before = p2w_live();
        let r2 = p2w_add_assign(r, big);
        assert_ne!(r2, r, "grow reallocates");
        assert_eq!(shown(r2), "abcdefghijklmnop");
        assert_eq!(p2w_live(), live_before, "+1 new, -1 consumed old");
        // ...and the slack means the NEXT append is in place again.
        let more = str_val("qr");
        let r3 = p2w_add_assign(r2, more);
        assert_eq!(r3, r2, "slack absorbs the next append");
        assert_eq!(shown(r3), "abcdefghijklmnopqr");
        // Aliased: rc 2 -> copy path; the alias keeps the original bytes.
        p2w_retain(r3); // simulate a second binding
        let tail = str_val("!!");
        let r4 = p2w_add_assign(r3, tail);
        assert_ne!(r4, r3);
        assert_eq!(shown(r4), "abcdefghijklmnopqr!!");
        assert_eq!(shown(r3), "abcdefghijklmnopqr", "alias untouched");
        // Self-append (a == b at rc 1) must copy, not smear.
        let z = str_val("xy");
        let r5 = p2w_add_assign(z, z);
        assert_eq!(shown(r5), "xyxy");
        // Numeric fallback: consumed old is an inline no-op.
        assert_eq!(p2w_add_assign(p2w_int(2), p2w_int(3)), p2w_int(5));
        p2w_release(r3);
        p2w_release(r4);
        p2w_release(r5);
        p2w_release(tail);
        p2w_release(more);
        p2w_release(suffix);
        p2w_release(str_val("")); // keep the guard's balance obvious
    }

    #[test]
    fn add_assign_extends_unique_lists_in_place() {
        let _g = heap_guard();
        let xs = p2w_list_new();
        p2w_list_append(xs, p2w_int(1));
        let ys = p2w_list_new();
        p2w_list_append(ys, p2w_int(2));
        p2w_list_append(ys, p2w_int(3));
        let r = p2w_add_assign(xs, ys);
        assert_eq!(r, xs, "unique list extends in place");
        assert_eq!(shown(r), "[1, 2, 3]");
        assert_eq!(shown(ys), "[2, 3]", "b is borrowed, untouched");
        p2w_release(r);
        p2w_release(ys);
        assert_eq!(p2w_live(), 0);
    }

    #[test]
    fn peak_tracks_the_high_water_mark() {
        let _g = heap_guard();
        assert_eq!(p2w_peak(), 0);
        let a = str_val("one");
        let b = str_val("two");
        assert_eq!(p2w_peak(), 2);
        p2w_release(a);
        p2w_release(b);
        // Peak stays at the high-water mark even after everything is freed...
        assert_eq!(p2w_live(), 0);
        assert_eq!(p2w_peak(), 2);
        // ...and a lone later allocation doesn't raise it.
        let c = str_val("three");
        assert_eq!(p2w_peak(), 2);
        p2w_release(c);
    }

    #[test]
    fn floats_print_inside_containers_and_release() {
        let _g = heap_guard();
        let xs = p2w_list_new();
        p2w_list_append(xs, p2w_float(1.5));
        p2w_list_append(xs, p2w_int(2));
        assert_eq!(shown(xs), "[1.5, 2]");
        // releasing the list frees the boxed float child too (no leak).
        let before = unsafe { CURSOR };
        p2w_release(xs);
        heap_reset();
        let _ = before; // arena reset reclaims; the release path must not trap
    }

    #[test]
    fn strings_alloc_print_len_index_concat_eq() {
        let _g = heap_guard();
        let hi = str_val("hi");
        assert_eq!(shown(hi), "hi");
        assert_eq!(as_int(p2w_len(hi)), 2);
        // index -> 1-char string
        assert_eq!(shown(p2w_index(hi, p2w_int(0))), "h");
        assert_eq!(shown(p2w_index(hi, p2w_int(-1))), "i");
        // concat
        let ab = p2w_add(str_val("a"), str_val("b"));
        assert_eq!(shown(ab), "ab");
        // equality is by contents, not identity
        assert_eq!(p2w_eq(str_val("hi"), str_val("hi")), V_TRUE);
        assert_eq!(p2w_ne(str_val("hi"), str_val("ho")), V_TRUE);
        // truthiness: empty string is falsy
        assert!(p2w_truthy(str_val("x")));
        assert!(!p2w_truthy(str_val("")));
    }

    #[test]
    fn refcount_release_frees_and_reuses() {
        let _g = heap_guard();
        let a = str_val("abcd"); // some block
        let off_a = a as usize;
        p2w_retain(a); // rc 1 -> 2
        p2w_release(a); // rc 2 -> 1 (still alive)
        assert_eq!(shown(a), "abcd");
        p2w_release(a); // rc 1 -> 0: freed
        // A new same-size allocation reuses the freed block (proves free works).
        let b = str_val("wxyz");
        assert_eq!(b as usize, off_a, "freed block should be reused");
        assert_eq!(shown(b), "wxyz");
    }

    #[test]
    fn inline_values_ignore_retain_release() {
        // retain/release on int/bool/None are safe no-ops.
        let n = p2w_int(7);
        p2w_retain(n);
        p2w_release(n);
        assert_eq!(as_int(n), 7);
        p2w_release(p2w_none());
        p2w_release(p2w_bool(1));
    }

    #[test]
    fn lists_build_index_set_len_print_concat() {
        let _g = heap_guard();
        let xs = p2w_list_new();
        p2w_list_append(xs, p2w_int(1));
        p2w_list_append(xs, p2w_int(2));
        p2w_list_append(xs, p2w_int(3));
        assert_eq!(as_int(p2w_len(xs)), 3);
        assert_eq!(as_int(p2w_index(xs, p2w_int(0))), 1);
        assert_eq!(as_int(p2w_index(xs, p2w_int(-1))), 3); // negative index
        p2w_setindex(xs, p2w_int(1), p2w_int(9));
        assert_eq!(as_int(p2w_index(xs, p2w_int(1))), 9);
        assert_eq!(shown(xs), "[1, 9, 3]");
        // strings inside a list print quoted (repr form)
        let ys = p2w_list_new();
        p2w_list_append(ys, str_val("hi"));
        assert_eq!(shown(ys), "['hi']");
        // concatenation
        assert_eq!(as_int(p2w_len(p2w_add(xs, ys))), 4);
        assert!(p2w_truthy(xs));
        assert!(!p2w_truthy(p2w_list_new())); // empty list is falsy
    }

    #[test]
    fn list_growth_keeps_object_offset_stable() {
        let _g = heap_guard();
        let xs = p2w_list_new();
        let off = xs as usize;
        for i in 0..50 {
            p2w_list_append(xs, p2w_int(i)); // forces several reallocs of the buffer
        }
        assert_eq!(xs as usize, off, "append must not move the list object");
        assert_eq!(as_int(p2w_len(xs)), 50);
        assert_eq!(as_int(p2w_index(xs, p2w_int(49))), 49);
    }

    #[test]
    fn dicts_set_get_update_len_print() {
        let _g = heap_guard();
        let d = p2w_dict_new();
        p2w_setindex(d, str_val("a"), p2w_int(1));
        p2w_setindex(d, str_val("b"), p2w_int(2));
        assert_eq!(as_int(p2w_len(d)), 2);
        assert_eq!(as_int(p2w_index(d, str_val("a"))), 1);
        // updating an existing key doesn't add a pair
        p2w_setindex(d, str_val("a"), p2w_int(9));
        assert_eq!(as_int(p2w_len(d)), 2);
        assert_eq!(as_int(p2w_index(d, str_val("a"))), 9);
        assert_eq!(shown(d), "{'a': 9, 'b': 2}"); // insertion order preserved
    }

    #[test]
    fn for_each_iterates_list_string_dict() {
        let _g = heap_guard();
        let xs = p2w_list_new();
        for i in 1..=3 {
            p2w_list_append(xs, p2w_int(i));
        }
        assert_eq!(collect_ints(xs), vec![1, 2, 3]);

        // string -> characters
        let it = p2w_iter(str_val("ab"));
        let mut s = String::new();
        while p2w_iter_has(it) {
            s.push_str(&shown(p2w_iter_next(it)));
        }
        assert_eq!(s, "ab");

        // dict -> keys, in insertion order
        let d = p2w_dict_new();
        p2w_setindex(d, p2w_int(10), p2w_int(0));
        p2w_setindex(d, p2w_int(20), p2w_int(0));
        assert_eq!(collect_ints(d), vec![10, 20]);
    }

    #[test]
    fn methods_append_and_pop() {
        let _g = heap_guard();
        let xs = p2w_list_new();
        unsafe {
            p2w_method1(xs, "append".as_ptr(), 6, p2w_int(1));
            p2w_method1(xs, "append".as_ptr(), 6, p2w_int(2));
            p2w_method1(xs, "append".as_ptr(), 6, p2w_int(3));
        }
        assert_eq!(as_int(p2w_len(xs)), 3);
        let last = unsafe { p2w_method0(xs, "pop".as_ptr(), 3) };
        assert_eq!(as_int(last), 3);
        assert_eq!(as_int(p2w_len(xs)), 2);
        // pop(0) shifts the tail down
        let first = unsafe { p2w_method1(xs, "pop".as_ptr(), 3, p2w_int(0)) };
        assert_eq!(as_int(first), 1);
        assert_eq!(as_int(p2w_len(xs)), 1);
        assert_eq!(as_int(p2w_index(xs, p2w_int(0))), 2);
    }

    #[test]
    fn list_equality_is_structural() {
        let _g = heap_guard();
        let a = p2w_list_new();
        p2w_list_append(a, p2w_int(1));
        p2w_list_append(a, p2w_int(2));
        let b = p2w_list_new();
        p2w_list_append(b, p2w_int(1));
        p2w_list_append(b, p2w_int(2));
        assert_eq!(p2w_eq(a, b), V_TRUE);
        p2w_list_append(b, p2w_int(3));
        assert_eq!(p2w_eq(a, b), V_FALSE);
    }

    #[test]
    fn releasing_a_list_releases_its_children() {
        let _g = heap_guard();
        let xs = p2w_list_new();
        let a = str_val("alpha"); // rc 1, transferred to the list on append
        p2w_list_append(xs, a);
        p2w_retain(a); // rc 2: the list + our test handle
        p2w_release(xs); // frees buffer/object and releases children: a 2 -> 1
        assert_eq!(shown(a), "alpha"); // still alive because we hold a ref
        p2w_release(a); // 1 -> 0: now freed (no leak)
    }

    #[test]
    fn setindex_releases_the_replaced_element() {
        let _g = heap_guard();
        let xs = p2w_list_new();
        let old = str_val("old");
        p2w_list_append(xs, old); // list owns `old`
        p2w_retain(old); // rc 2: list + handle
        p2w_setindex(xs, p2w_int(0), str_val("new")); // releases old (2 -> 1)
        assert_eq!(shown(old), "old"); // survives via our handle
        assert_eq!(shown(p2w_index(xs, p2w_int(0))), "new");
        p2w_release(old);
    }

    #[test]
    fn index_returns_an_owned_reference() {
        let _g = heap_guard();
        let xs = p2w_list_new();
        let s = str_val("hi");
        p2w_list_append(xs, s); // list owns s (rc 1)
        let got = p2w_index(xs, p2w_int(0)); // owned -> retains, rc 2
        assert_eq!(got, s);
        assert_eq!(rd(got as usize + 4), 2, "index hands back an owned ref");
    }

    // Note: division-by-zero / type errors call `trap`, which on the device
    // halts and in tests panics — but it panics *through* an `extern "C"` fn,
    // which aborts rather than unwinds, so it can't be asserted with
    // `#[should_panic]`. The detection logic itself is straightforward; the
    // happy-path arithmetic tests above cover the encode/decode round trip.
}
