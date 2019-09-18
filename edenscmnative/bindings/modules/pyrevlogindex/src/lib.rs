// Copyright 2019 Facebook, Inc.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

#![allow(non_camel_case_types)]

use cpython::*;
use cpython_ext::SimplePyBuf;
use dag::spanset::{Id, SpanSet};
use pydag::Spans;
use std::cell::RefCell;

// XXX: The revlogindex is a temporary solution before migrating to
// segmented changelog. It is here to experiment breaking changes with
// revlog, incluing:
//
// - Redefine "head()" to only return remotenames and tracked draft heads.
// - Get rid of "filtered revs" and "repo view" layer entirely.
// - Switch phases to be defined by heads (remotenames), instead of roots.

pub fn init_module(py: Python, package: &str) -> PyResult<PyModule> {
    let name = [package, "revlogindex"].join(".");
    let m = PyModule::new(py, &name)?;
    m.add_class::<revlogindex>(py)?;
    Ok(m)
}

py_class!(class revlogindex |py| {
    data changelogi: RevlogIndex;

    def __new__(_cls, changelogi: &PyObject) -> PyResult<Self> {
        let changelogi = RevlogIndex {
            data: SimplePyBuf::new(py, changelogi),
            inserted: RefCell::new(Vec::new()),
        };
        Self::create_instance(py, changelogi)
    }

    /// Given public and draft head revision numbers, calculate the "phase sets".
    /// Return (publicset, draftset).
    def phasesets(&self, publicheads: Vec<u32>, draftheads: Vec<u32>) -> PyResult<(Spans, Spans)> {
        let revlog = self.changelogi(py);
        let mut draft_set = SpanSet::empty();
        let mut public_set = SpanSet::empty();

        // Used internally. Different from "phases.py".
        #[repr(u8)]
        #[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq)]
        enum Phase {
            Unspecified,
            Draft,
            Public,
        }
        impl Phase {
            fn max(self, other: Phase) -> Phase {
                if self > other { self } else {other}
            }
        }

        let mut phases = vec![Phase::Unspecified; revlog.len()];
        for rev in draftheads {
            phases[rev as usize] = Phase::Draft;
        }
        for rev in publicheads {
            phases[rev as usize] = Phase::Public;
        }

        for rev in (0..revlog.len()).rev() {
            let phase = phases[rev as usize];
            match phase {
                Phase::Public => public_set.push(rev as Id),
                Phase::Draft => draft_set.push(rev as Id),
                // Do not track "unknown" explicitly. This is future-proof,
                // since tracking "unknown" explicitly is quite expensive
                // with the new "dag" abstraction.
                Phase::Unspecified => (),
            }
            for parent_rev in revlog.parents(rev as u32) {
                // Propagate phases from this rev to its parents.
                phases[parent_rev as usize] = phases[parent_rev as usize].max(phase);
            }
        }
        Ok((Spans(public_set), Spans(draft_set)))
    }

    /// Get parent revisions.
    def parentrevs(&self, rev: u32) -> PyResult<Vec<u32>> {
        let revlog = self.changelogi(py);
        Ok(revlog.parents(rev))
    }

    /// Insert a new revision that hasn't been written to disk.
    /// Used by revlog._addrevision.
    def insert(&self, parents: Vec<u32>) -> PyResult<PyObject> {
        let revlog = self.changelogi(py);
        revlog.insert(parents);
        Ok(py.None())
    }

    def __len__(&self) -> PyResult<usize> {
        let revlog = self.changelogi(py);
        Ok(revlog.len())
    }
});

/// Minimal code to read the DAG (i.e. parents) stored in non-inlined revlog.
struct RevlogIndex {
    // Content of revlog-name.i (ex. 00changelog.i).
    data: SimplePyBuf<RevlogEntry>,

    // Inserted entries that are not flushed to disk.
    inserted: RefCell<Vec<Vec<u32>>>,
}

/// Revlog entry. See "# index ng" in revlog.py.
#[allow(dead_code)]
#[repr(packed)]
#[derive(Copy, Clone)]
struct RevlogEntry {
    offset_flags: u64,
    compressed: i32,
    len: i32,
    base: i32,
    link: i32,
    p1: i32,
    p2: i32,
    node: [u8; 32],
}

impl RevlogIndex {
    /// Revisions in total.
    fn len(&self) -> usize {
        let inserted = self.inserted.borrow();
        self.data_len() + inserted.len()
    }

    /// Revisions stored in the original revlog index.
    fn data_len(&self) -> usize {
        self.data.as_ref().len()
    }

    /// Get parent revisions.
    fn parents(&self, rev: u32) -> Vec<u32> {
        let data_len = self.data_len();
        if rev >= data_len as u32 {
            let inserted = self.inserted.borrow();
            return inserted[rev as usize - data_len].clone();
        }

        let data = self.data.as_ref();
        let p1 = i32::from_be(data[rev as usize].p1);
        let p2 = i32::from_be(data[rev as usize].p2);
        if p1 == -1 {
            // p1 == -1 but p2 != -1 is illegal for changelog (but possible
            // for filelog with copy information).
            assert!(p2 == -1);
            Vec::new()
        } else if p2 == -1 {
            assert!((p1 as u32) < rev);
            vec![p1 as u32]
        } else {
            assert!((p1 as u32) < rev);
            assert!((p2 as u32) < rev);
            vec![p1 as u32, p2 as u32]
        }
    }

    /// Insert a new revision with given parents at the end.
    fn insert(&self, parents: Vec<u32>) {
        let mut inserted = self.inserted.borrow_mut();
        inserted.push(parents);
    }
}
