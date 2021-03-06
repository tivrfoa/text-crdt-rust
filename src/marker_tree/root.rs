use super::*;

use std::ptr;

impl MarkerTree {
    pub fn new() -> Pin<Box<Self>> {
        let mut tree = Box::pin(unsafe { Self {
            count: 0,
            root: Box::pin(Node::new()),
            _pin: marker::PhantomPinned,
        } });

        unsafe {
            let ptr = tree.as_mut().get_unchecked_mut();
            *ptr.root.get_parent_mut() = ParentPtr::Root(NonNull::new_unchecked(ptr));
        }

        tree
    }

    pub fn cursor_at_pos<'a>(self: &'a Pin<Box<Self>>, raw_pos: u32, stick_end: bool) -> Cursor<'a> {
        // let mut node: *const Node = &*self.root.as_ref().unwrap().as_ref();
        let mut node: *const Node = &*self.root.as_ref();
        let mut offset_remaining = raw_pos;
        unsafe {
            while let Node::Internal(data) = &*node {
                let (new_offset_remaining, next) = data.get_child(offset_remaining, stick_end).expect("Internal consistency violation");
                offset_remaining = new_offset_remaining;
                node = next.get_ref();
            };

            let node = (*node).unwrap_leaf();
            let (idx, offset_remaining) = node.find_offset(offset_remaining, stick_end)
            .expect("Element does not contain entry");

            Cursor {
                node: NonNull::new_unchecked(node as *const _ as *mut _),
                idx,
                offset: offset_remaining,
                _marker: marker::PhantomData
            }
        }
    }

    // Make room at the current cursor location, splitting the current element
    // if necessary (and recursively splitting the btree node if there's no
    // room). The gap will be filled with junk and must be immediately
    // overwritten. (The location of the gap is returned via the cursor.)
    unsafe fn make_space_in_leaf<F>(cursor: &mut Cursor, gap: usize, notify: &mut F)
        where F: FnMut(CRDTLocation, ClientSeq, NonNull<NodeLeaf>)
    {
        let mut node = cursor.node.as_mut();
        
        {
            // let mut entry = &mut node.0[cursor.idx];
            // let seq_len = entry.get_seq_len();
            let seq_len = node.data[cursor.idx].get_seq_len();

            // If we're at the end of the current entry, skip it.
            if cursor.offset == seq_len {
                cursor.offset = 0;
                cursor.idx += 1;
                // entry = &mut node.0[cursor.idx];
            }
        }
        
        let space_needed = if cursor.offset > 0 {
            // We'll need an extra space to split the node.
            gap + 1
        } else {
            gap
        };

        // TODO(opt): Consider caching this in each leaf.
        // let mut filled_entries = node.count_entries();
        let num_filled = node.len as usize;

        // Bail if we don't need to make space or we're trying to insert at the end.
        if space_needed == 0 { return; }
        if cursor.idx == num_filled && num_filled + space_needed <= NUM_ENTRIES {
            // There's room at the end of the leaf.
            debug_assert!(cursor.offset == 0);
            node.len += gap as u8;
            return;
        }

        if num_filled + space_needed > NUM_ENTRIES {
            // Split the entry in two. space_needed should always be 1 or 2, and
            // there needs to be room after splitting.
            debug_assert!(space_needed == 1 || space_needed == 2);
            debug_assert!(space_needed <= NUM_ENTRIES/2); // unnecessary but simplifies things.
            
            // By conventional btree rules, we should make sure each side of the
            // split has at least n/2 elements but in this case I don't think it
            // really matters. I'll do something reasonable that is clean and clear.
            if cursor.idx < NUM_ENTRIES/2 {
                // Put the new items at the end of the current node and
                // move everything afterward to a new node.
                let split_point = if cursor.offset == 0 { cursor.idx } else { cursor.idx + 1 };
                node.split_at(split_point, notify);
            } else {
                // Split in the middle of the current node. This involves a
                // little unnecessary copying - because we're copying the
                // elements into the new node then we'll split (and copy them
                // again) below but its ok for now. Memcpy is fast.

                // The other option here would be to use the index as a split
                // point and add padding into the new node to leave space.
                cursor.node = node.split_at(NUM_ENTRIES/2, notify);
                node = cursor.node.as_mut();
                cursor.idx -= NUM_ENTRIES/2;
            }
        }

        // There's room in the node itself now. We need to reshuffle.
        let src_idx = cursor.idx;
        let dest_idx = src_idx + space_needed;
        let num_copied = node.len as usize - src_idx;
        node.len += space_needed as u8;

        if num_copied > 0 {
            ptr::copy(&node.data[src_idx], &mut node.data[dest_idx], num_copied);
        }
        
        // Tidy up the edges
        if cursor.offset > 0 {
            debug_assert!(num_copied > 0);
            node.data[src_idx].keep_start(cursor.offset);
            node.data[dest_idx].keep_end(cursor.offset);
            cursor.idx += 1;
            cursor.offset = 0;
        }
    }

    /**
     * Insert a new CRDT insert / delete at some raw position in the document
     */
    pub fn insert<F>(self: &Pin<Box<Self>>, mut cursor: Cursor, len: ClientSeq, new_loc: CRDTLocation, mut notify: F)
        where F: FnMut(CRDTLocation, ClientSeq, NonNull<NodeLeaf>)
    {
        let expected_size = self.count + len;

        if cfg!(debug_assertions) {
            self.as_ref().get_ref().check();
        }

        // First walk down the tree to find the location.
        // let mut node = self;

        // let mut cursor = self.cursor_at_pos(raw_pos, true);
        unsafe {
            // Insert has 3 cases:
            // - 1. The entry can be extended. We can do this inline.
            // - 2. The inserted text is at the end an entry, but the entry cannot
            //   be extended. We need to add 1 new entry to the leaf.
            // - 3. The inserted text is in the middle of an entry. We need to
            //   split the entry and insert a new entry in the middle. We need
            //   to add 2 new entries.

            let old_len = cursor.node.as_ref().len;
            let old_entry = &mut cursor.node.as_mut().data[cursor.idx];

            // We also want case 2 if the node is brand new...
            if cursor.idx == 0 && old_len == 0 /*old_entry.loc.client == CLIENT_INVALID*/ {
                *old_entry = Entry {
                    loc: new_loc,
                    len: len as i32,
                };
                cursor.node.as_mut().len = 1;
                cursor.node.as_mut().update_parent_count(len as i32);
                notify(new_loc, len, cursor.node);
            } else if old_entry.len > 0 && old_entry.len as u32 == cursor.offset
                    && old_entry.loc.client == new_loc.client
                    && old_entry.loc.seq + old_entry.len as u32 == new_loc.seq {
                // Case 1 - Extend the entry.
                old_entry.len += len as i32;
                cursor.node.as_mut().update_parent_count(len as i32);
                notify(new_loc, len, cursor.node);
            } else {
                // Case 2 and 3.
                Self::make_space_in_leaf(&mut cursor, 1, &mut notify); // This will update len for us
                cursor.node.as_mut().data[cursor.idx] = Entry {
                    loc: new_loc,
                    len: len as i32
                };
                debug_assert!(cursor.node.as_ref().len >= 1);
                cursor.node.as_mut().update_parent_count(len as i32);
                notify(new_loc, len, cursor.node);
            }
        }

        if cfg!(debug_assertions) {
            // eprintln!("{:#?}", self.as_ref().get_ref());
            self.as_ref().get_ref().check();

            // And check the total size of the tree has grown by len.
            assert_eq!(expected_size, self.count);
        }
    }

    pub fn delete(&mut self, _raw_pos: u32) {
        unimplemented!("delete");
    }

    // Returns size.
    fn check_leaf(leaf: &NodeLeaf, expected_parent: ParentPtr) -> usize {
        assert_eq!(leaf.parent, expected_parent);
        
        let mut count: usize = 0;
        let mut done = false;
        let mut num: usize = 0;

        for e in &leaf.data[..] {
            if e.is_invalid() {
                done = true;
            } else {
                // Make sure there's no data after an invalid entry
                assert!(done == false, "Leaf contains gaps");
                count += e.get_text_len() as usize;
                num += 1;
            }
        }

        // An empty leaf is only valid if we're the root element.
        if let ParentPtr::Internal(_) = leaf.parent {
            assert!(count > 0, "Non-root leaf is empty");
        }

        assert_eq!(num, leaf.len as usize, "Cached leaf len does not match");

        count
    }
    
    // Returns size.
    fn check_internal(node: &NodeInternal, expected_parent: ParentPtr) -> usize {
        assert_eq!(node.parent, expected_parent);
        
        let mut count_total: usize = 0;
        let mut done = false;
        let mut child_type = None; // Make sure all the children have the same type.
        let self_parent = ParentPtr::Internal(NonNull::new(node as *const _ as *mut _).unwrap());

        for (child_count_expected, child) in &node.data[..] {
            if let Some(child) = child {
                // Make sure there's no data after an invalid entry
                assert!(done == false);

                let child_ref = child.as_ref().get_ref();

                let actual_type = match child_ref {
                    Node::Internal(_) => 1,
                    Node::Leaf(_) => 2,
                };
                // Make sure all children have the same type.
                if child_type.is_none() { child_type = Some(actual_type) }
                else { assert_eq!(child_type, Some(actual_type)); }

                // Recurse
                let count_actual = match child_ref {
                    Node::Leaf(n) => { Self::check_leaf(n, self_parent) },
                    Node::Internal(n) => { Self::check_internal(n, self_parent) },
                };

                // Make sure all the individual counts match.
                // if *child_count_expected as usize != count_actual {
                //     eprintln!("xxx {:#?}", node);
                // }
                assert_eq!(*child_count_expected as usize, count_actual, "Child node count does not match");
                count_total += count_actual;
            } else {
                done = true;
            }
        }

        count_total
    }

    pub fn check(&self) {
        // Check the parent of each node is its correct parent
        // Check the size of each node is correct up and down the tree
        let root = self.root.as_ref().get_ref();
        let expected_parent = ParentPtr::Root(NonNull::new(self as *const _ as *mut Self).unwrap());
        let expected_size = match root {
            Node::Internal(n) => { Self::check_internal(&n, expected_parent) },
            Node::Leaf(n) => { Self::check_leaf(&n, expected_parent) },
        };
        assert_eq!(self.count as usize, expected_size);
    }

    fn print_node(node: &Node, depth: usize) {
        for _ in 0..depth { eprint!("  "); }
        match node {
            Node::Internal(n) => {
                eprintln!("Internal {:?} (parent: {:?})", n as *const _, n.parent);
                let mut unused = 0;
                for (_, e) in &n.data[..] {
                    if let Some(e) = e {
                        Self::print_node(e.as_ref().get_ref(), depth + 1);
                    } else { unused += 1; }
                }

                if unused > 0 {
                    for _ in 0..=depth { eprint!("  "); }
                    eprintln!("({} empty places)", unused);
                }
            },
            Node::Leaf(n) => {
                eprintln!("Leaf {:?} (parent: {:?}) - {} filled", n as *const _, n.parent, n.count_entries());
            }
        }
    }

    pub fn print_ptr_tree(&self) {
        eprintln!("Tree count {} ptr {:?}", self.count, self as *const _);
        Self::print_node(self.root.as_ref().get_ref(), 1);
    }

    pub unsafe fn lookup_position(loc: CRDTLocation, ptr: NonNull<NodeLeaf>) -> u32 {
        // First make a cursor to the specified item
        let leaf = ptr.as_ref();
        let cursor = leaf.find(loc).expect("Position not in named leaf");
        cursor.get_pos()
    }
}
