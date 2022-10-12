//! Experimental GC, currently very simple generational GC.
//! Two generations: young and old
//! Full heap mark, then compection decision (young only, full collection, or no collection)
//! Based on the Motoko RTS mark & compact GC.

pub mod remembered_set;
#[cfg(debug_assertions)]
mod sanity_checks;
pub mod write_barrier;

use crate::gc::mark_compact::bitmap::{
    alloc_bitmap, free_bitmap, get_bit, iter_bits, set_bit, BITMAP_ITER_END,
};
use crate::gc::mark_compact::mark_stack::{
    alloc_mark_stack, free_mark_stack, pop_mark_stack, push_mark_stack,
};

use crate::constants::WORD_SIZE;
use crate::mem_utils::memcpy_words;
use crate::memory::Memory;
use crate::types::*;
use crate::visitor::{pointer_to_dynamic_heap, visit_pointer_fields};

use motoko_rts_macros::ic_mem_fn;

use self::write_barrier::REMEMBERED_SET;

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum Strategy {
    Young,
    Full,
}

#[ic_mem_fn(ic_only)]
unsafe fn schedule_experimental_gc<M: Memory>(mem: &mut M) {
    // 512 MiB slack for mark stack + allocation area for the next message
    let slack: u64 = 512 * 1024 * 1024;
    let heap_size_bytes: u64 =
        u64::from(crate::constants::WASM_HEAP_SIZE.as_u32()) * u64::from(WORD_SIZE);
    // Larger than necessary to keep things simple
    let max_bitmap_size_bytes = heap_size_bytes / 32;
    // NB. `max_live` is evaluated in compile time to a constant
    let max_live: Bytes<u64> = Bytes(heap_size_bytes - slack - max_bitmap_size_bytes);

    if super::should_do_gc(max_live) {
        experimental_gc(mem);
    }
}

#[ic_mem_fn(ic_only)]
unsafe fn experimental_gc<M: Memory>(mem: &mut M) {
    use crate::memory::ic;

    println!(100, "INFO: Experimental GC starts ...");

    #[cfg(debug_assertions)]
    sanity_checks::verify_snapshot(
        ic::get_aligned_heap_base(),
        ic::LAST_HP,
        ic::HP,
        ic::get_static_roots(),
    );

    experimental_gc_internal(
        mem,
        ic::get_aligned_heap_base(),
        // get_hp
        || ic::HP as usize,
        // get_last_hp
        || ic::LAST_HP as usize,
        // set_hp
        |hp| ic::HP = hp,
        ic::get_static_roots(),
        crate::continuation_table::continuation_table_loc(),
        // note_live_size
        |live_size| ic::MAX_LIVE = ::core::cmp::max(ic::MAX_LIVE, live_size),
        // note_reclaimed
        |reclaimed| ic::RECLAIMED += Bytes(u64::from(reclaimed.as_u32())),
        decide_strategy(ic::HP as usize),
    );
    ic::LAST_HP = ic::HP;

    #[cfg(debug_assertions)]
    sanity_checks::take_snapshot(mem, ic::HP);

    write_barrier::init_write_barrier(mem);

    println!(100, "INFO: Experimental GC stops ...");
}

// TODO: Only for testing, improve strategy decision later
static mut RUN: usize = 0;

unsafe fn decide_strategy(hp: usize) -> Strategy {
    RUN = RUN + 1;
    if RUN % 3 == 0 {
        return Strategy::Full
    }

    const CRITICAL_MEMORY_LIMIT: usize = (4096 - 256) * 1024 * 1024;
    // TODO: Refine, run full GC at certain low frequency of young collection runs
    if hp >= CRITICAL_MEMORY_LIMIT {
        println!(100, "CRITICAL MEMORY LIMIT");
        Strategy::Full
    } else {
        Strategy::Young
    }
}

pub unsafe fn experimental_gc_internal<
    M: Memory,
    GetHp: Fn() -> usize,
    GetLastHp: Fn() -> usize,
    SetHp: Fn(u32),
    NoteLiveSize: Fn(Bytes<u32>),
    NoteReclaimed: Fn(Bytes<u32>),
>(
    mem: &mut M,
    heap_base: u32,
    get_hp: GetHp,
    get_last_hp: GetLastHp,
    set_hp: SetHp,
    static_roots: Value,
    continuation_table_ptr_loc: *mut Value,
    note_live_size: NoteLiveSize,
    note_reclaimed: NoteReclaimed,
    strategy: Strategy,
) {
    let limits = Limits {
        base: heap_base as usize,
        last_free: core::cmp::max(get_last_hp(), heap_base as usize), // max because of aligned heap base
        free: get_hp(),
    };

    let roots = Roots {
        static_roots,
        continuation_table_ptr_loc,
    };

    let heap = Heap { mem, limits, roots };

    let mut gc = ExperimentalGC {
        heap,
        marked_space: 0,
        strategy,
    };

    let old_hp = get_hp() as u32;

    assert_eq!(heap_base % 32, 0);

    gc.mark_compact(set_hp);

    let reclaimed = old_hp - (get_hp() as u32);
    note_reclaimed(Bytes(reclaimed));

    let live = get_hp() as u32 - heap_base;
    note_live_size(Bytes(live));
}

struct Heap<'a, M: Memory> {
    pub mem: &'a mut M,
    pub limits: Limits,
    pub roots: Roots,
}

struct Roots {
    pub static_roots: Value,
    pub continuation_table_ptr_loc: *mut Value,
}

struct Limits {
    pub base: usize,
    pub last_free: usize,
    pub free: usize,
}

struct ExperimentalGC<'a, M: Memory> {
    heap: Heap<'a, M>,
    marked_space: usize,
    strategy: Strategy,
}

impl<'a, M: Memory> ExperimentalGC<'a, M> {
    unsafe fn mark_compact<SetHp: Fn(u32)>(&mut self, set_hp: SetHp) {
        println!(100, "STRATEGY: {:?}", self.strategy);

        let heap_end = self.heap.limits.free as u32;
        let mem_size = Bytes(heap_end - self.heap.limits.base as u32);

        alloc_bitmap(
            self.heap.mem,
            mem_size,
            self.heap.limits.base as u32 / WORD_SIZE,
        );
        alloc_mark_stack(self.heap.mem);

        self.mark_phase();

        println!(
            100,
            "MARKED {} OF {} RATIO {:.3}",
            self.marked_space,
            self.generation_size(),
            self.survival_rate()
        );

        let mut free = heap_end;
        if self.is_compaction_beneficial() {
            self.thread_backward_phase();
            free = self.move_phase() as u32;
        }
        set_hp(free);

        free_mark_stack();
        free_bitmap();
    }

    fn is_compaction_beneficial(&self) -> bool {
        self.survival_rate() < 0.95
    }

    fn generation_size(&self) -> usize {
        let base = match self.strategy {
            Strategy::Young => self.heap.limits.last_free,
            Strategy::Full => self.heap.limits.base,
        };
        self.heap.limits.free - base
    }

    fn survival_rate(&self) -> f64 {
        self.marked_space as f64 / self.generation_size() as f64
    }

    unsafe fn mark_phase(&mut self) {
        self.marked_space = 0;
        self.mark_root_set();
        self.mark_all_reachable();
    }

    unsafe fn mark_root_set(&mut self) {
        self.mark_static_roots();

        if (*self.heap.roots.continuation_table_ptr_loc).is_ptr() {
            self.mark_object(*self.heap.roots.continuation_table_ptr_loc);
        }

        if self.strategy == Strategy::Young {
            self.mark_additional_young_root_set();
        }
    }

    unsafe fn mark_static_roots(&mut self) {
        let root_array = self.heap.roots.static_roots.as_array();

        // Static objects are not in the dynamic heap so don't need marking.
        for i in 0..root_array.len() {
            let obj = root_array.get(i).as_obj();
            // Root array should only have pointers to other static MutBoxes
            debug_assert_eq!(obj.tag(), TAG_MUTBOX); // check tag
            debug_assert!((obj as usize) < self.heap.limits.base); // check that MutBox is static
            self.mark_root_mutbox_fields(obj as *mut MutBox);
        }
    }

    unsafe fn mark_additional_young_root_set(&mut self) {
        match &REMEMBERED_SET {
            None => panic!("Write barrier is not activated"),
            Some(remembered_set) => {
                let mut iterator = remembered_set.iterate();
                while iterator.has_next() {
                    let location = iterator.current().get_raw();
                    let object = *(location as *mut Value);
                    // Check whether the location still refers to young object as this may have changed
                    // due to subsequent writes to that location after the write barrier recording.
                    if object.is_ptr() && (object.get_raw() as usize) >= self.heap.limits.last_free
                    {
                        self.mark_object(object);
                    }
                    iterator.next();
                }
            }
        }
    }

    unsafe fn mark_object(&mut self, object: Value) {
        let pointer = object.get_ptr() as u32;
        if self.strategy == Strategy::Young && pointer < self.heap.limits.last_free as u32 {
            return;
        }
        debug_assert_eq!(pointer % WORD_SIZE, 0);

        let obj_idx = pointer / WORD_SIZE;
        if get_bit(obj_idx) {
            return;
        }
        set_bit(obj_idx);

        push_mark_stack(self.heap.mem, pointer as usize, object.tag());
        let obj_size = object_size(pointer as usize).to_bytes().as_usize();
        self.marked_space += obj_size;
    }

    unsafe fn mark_all_reachable(&mut self) {
        while let Some((obj, tag)) = pop_mark_stack() {
            self.mark_fields(obj as *mut Obj, tag)
        }
    }

    unsafe fn mark_fields(&mut self, obj: *mut Obj, obj_tag: Tag) {
        visit_pointer_fields(
            self,
            obj,
            obj_tag,
            self.heap.limits.base,
            |gc, field_addr| {
                let field_value = *field_addr;
                gc.mark_object(field_value);
            },
            |gc, slice_start, arr| {
                const SLICE_INCREMENT: u32 = 127;
                debug_assert!(SLICE_INCREMENT >= TAG_ARRAY_SLICE_MIN);
                if arr.len() - slice_start > SLICE_INCREMENT {
                    let new_start = slice_start + SLICE_INCREMENT;
                    // push an entire (suffix) array slice
                    push_mark_stack(gc.heap.mem, arr as usize, new_start);
                    new_start
                } else {
                    arr.len()
                }
            },
        );
    }

    /// Specialized version of `mark_fields` for root `MutBox`es.
    unsafe fn mark_root_mutbox_fields(&mut self, mutbox: *mut MutBox) {
        let field_addr = &mut (*mutbox).field;
        if pointer_to_dynamic_heap(field_addr, self.heap.limits.base) {
            self.mark_object(*field_addr);
        }
    }

    unsafe fn should_be_threaded(&self, obj: *mut Obj) -> bool {
        let address = obj as usize;
        match self.strategy {
            Strategy::Young => address >= self.heap.limits.last_free,
            Strategy::Full => true,
        }
    }

    unsafe fn thread_backward_phase(&mut self) {
        self.thread_all_backward_pointers();

        // For static root, also forward pointers are threaded.
        // Therefore, this must happen after the heap traversal for backwards pointer threading.
        self.thread_static_roots();

        if (*self.heap.roots.continuation_table_ptr_loc).is_ptr() {
            // Similar to `mark_root_mutbox_fields`, `continuation_table_ptr_loc` is in static heap so
            // it will be readable when we unthread the continuation table
            self.thread(self.heap.roots.continuation_table_ptr_loc);
        }
    }

    unsafe fn thread_static_roots(&self) {
        let root_array = self.heap.roots.static_roots.as_array();

        for i in 0..root_array.len() {
            let obj = root_array.get(i).as_obj();
            // Root array should only have pointers to other static MutBoxes
            debug_assert_eq!(obj.tag(), TAG_MUTBOX); // check tag
            debug_assert!((obj as usize) < self.heap.limits.base); // check that MutBox is static
            self.thread_root_mutbox_fields(obj as *mut MutBox);
        }
    }

    unsafe fn thread_root_mutbox_fields(&self, mutbox: *mut MutBox) {
        let field_addr = &mut (*mutbox).field;
        if pointer_to_dynamic_heap(field_addr, self.heap.limits.base) {
            // It's OK to thread forward pointers here as the static objects won't be moved, so we will
            // be able to unthread objects pointed by these fields later.
            self.thread(field_addr);
        }
    }

    unsafe fn thread_all_backward_pointers(&mut self) {
        let mut bitmap_iter = iter_bits();
        if self.strategy == Strategy::Young {
            bitmap_iter.advance(self.heap.limits.last_free as u32);
        }
        let mut bit = bitmap_iter.next();
        while bit != BITMAP_ITER_END {
            let obj = (bit * WORD_SIZE) as *mut Obj;
            let tag = obj.tag();

            self.thread_backward_pointer_fields(obj, tag);

            bit = bitmap_iter.next();
        }
    }

    unsafe fn thread_backward_pointer_fields(&mut self, obj: *mut Obj, obj_tag: Tag) {
        assert!(obj_tag < TAG_ARRAY_SLICE_MIN);
        visit_pointer_fields(
            self,
            obj,
            obj_tag,
            self.heap.limits.base,
            |gc, field_addr| {
                let field_value = *field_addr;

                // Thread if backwards or self pointer
                if field_value.get_ptr() <= obj as usize {
                    gc.thread(field_addr);
                }
            },
            |_, slice_start, arr| {
                debug_assert!(slice_start == 0);
                arr.len()
            },
        );
    }

    /// Linearly scan the heap, for each live object:
    ///
    /// - Mark step threads all backwards pointers and pointers from roots, so unthread to update those
    ///   pointers to the objects new location.
    ///
    /// - Move the object
    ///
    /// - Thread forward pointers of the object
    ///
    /// Returns the new free pointer
    ///
    unsafe fn move_phase(&mut self) -> usize {
        let mut free = self.heap.limits.base;

        let mut bitmap_iter = iter_bits();
        if self.strategy == Strategy::Young {
            self.thread_old_generation_pointers();
            bitmap_iter.advance(self.heap.limits.last_free as u32);
            free = self.heap.limits.last_free;
        }
        let mut bit = bitmap_iter.next();
        while bit != BITMAP_ITER_END {
            let p = (bit * WORD_SIZE) as *mut Obj;
            let p_new = free;

            // Update backwards references to the object's new location and restore object header
            self.unthread(p, p_new);

            // Move the object
            let p_size_words = object_size(p as usize);
            if p_new as usize != p as usize {
                memcpy_words(p_new as usize, p as usize, p_size_words);
                // Update forward address
                let new_obj = p_new as *mut Obj;
                (*new_obj).forward = Value::from_ptr(p_new as usize);
            }

            free += p_size_words.to_bytes().as_usize();

            // Thread forward pointers of the object, even if not moved
            self.thread_fwd_pointers(p_new as *mut Obj);

            bit = bitmap_iter.next();
        }

        free
    }

    // Thread forward pointers in old generation leading to young generation
    unsafe fn thread_old_generation_pointers(&mut self) {
        // TODO: implement this more efficiently, by focussing on remembered set
        let mut pointer = self.heap.limits.base;
        while pointer < self.heap.limits.last_free {
            let object = pointer as *mut Obj;
            let tag = (*object).tag;
            assert!(tag < TAG_FREE_SPACE);
            if tag != TAG_ONE_WORD_FILLER {
                assert!((*object).forward.is_null() || (*object).forward.get_ptr() == pointer);
                self.thread_fwd_pointers(object);
            }
            let object_size = object_size(pointer).to_bytes().as_usize();
            pointer += object_size;
        }
    }

    /// Thread forward pointers in object
    unsafe fn thread_fwd_pointers(&mut self, obj: *mut Obj) {
        visit_pointer_fields(
            self,
            obj,
            obj.tag(),
            self.heap.limits.base,
            |gc, field_addr| {
                if (*field_addr).get_ptr() > obj as usize {
                    gc.thread(field_addr)
                }
            },
            |_, _, arr| arr.len(),
        );
    }

    /// Thread a pointer field
    unsafe fn thread(&self, field: *mut Value) {
        // Store pointed object's header in the field, field address in the pointed object's header
        let pointed = (*field).get_ptr() as *mut Obj;
        if self.should_be_threaded(pointed) {
            let pointed_header = pointed.tag();
            *field = Value::from_raw(pointed_header);
            (*pointed).tag = field as u32;
        }
    }

    /// Unthread all references at given header, replacing with `new_loc`. Restores object header.
    unsafe fn unthread(&self, obj: *mut Obj, new_loc: usize) {
        assert!(self.should_be_threaded(obj));
        let mut header = obj.tag();

        // All objects and fields are word-aligned, and tags have the lowest bit set, so use the lowest
        // bit to distinguish a header (tag) from a field address.
        while header & 0b1 == 0 {
            let tmp = (header as *const Obj).tag();
            (*(header as *mut Value)) = Value::from_ptr(new_loc);
            header = tmp;
        }

        // At the end of the chain is the original header for the object
        debug_assert!(header >= TAG_OBJECT && header <= TAG_NULL);

        (*obj).tag = header;
    }
}
