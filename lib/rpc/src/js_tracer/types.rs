use alloy::primitives::{Address, B256, Bytes, U256};
use std::{
    cell::{RefCell, RefMut},
    collections::HashMap,
    hash::Hash,
    rc::Rc,
};

#[derive(Clone, Copy, Debug)]
pub(crate) enum CreateType {
    Create,
    Create2,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum TracerMethod {
    Setup,
    Enter,
    Exit,
    Step,
    Fault,
    Result,
    Write,
    StorageRead,
}

impl TracerMethod {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            TracerMethod::Setup => "setup",
            TracerMethod::Enter => "enter",
            TracerMethod::Exit => "exit",
            TracerMethod::Step => "step",
            TracerMethod::Fault => "fault",
            TracerMethod::Result => "result",
            TracerMethod::Write => "write",
            TracerMethod::StorageRead => "storage_read",
        }
    }
}

#[derive(Debug)]
pub struct OverlayEntry<V> {
    pub(crate) value: V,
    pub(crate) committed: bool,
    pub(crate) previous: Option<V>,
}

impl<V: Clone> Clone for OverlayEntry<V> {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            committed: self.committed,
            previous: self.previous.clone(),
        }
    }
}

impl<V> OverlayEntry<V> {
    pub(crate) fn new_pending(value: V) -> Self {
        Self {
            value,
            committed: false,
            previous: None,
        }
    }
}

pub type StorageOverlay = HashMap<(Address, B256), OverlayEntry<B256>>;
pub type CodeOverlay = HashMap<Address, OverlayEntry<Option<Vec<u8>>>>;
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BalanceDelta {
    pub added: U256,
    pub removed: U256,
}

#[derive(Clone, Default, Debug)]
pub struct SelfdestructEntry {
    pub is_deployed_in_current_tx: bool,
    pub is_marked_for_selfdestruct: bool,
}

impl BalanceDelta {
    pub fn credit(&mut self, amount: U256) -> anyhow::Result<()> {
        if amount == U256::ZERO {
            return Ok(());
        }

        let (new_total, overflow) = self.added.overflowing_add(amount);
        if overflow {
            anyhow::bail!("Balance credit overflow");
        }

        self.added = new_total;

        Ok(())
    }

    pub fn debit(&mut self, amount: U256) -> anyhow::Result<()> {
        if amount == U256::ZERO {
            return Ok(());
        }

        let (new_total, overflow) = self.removed.overflowing_add(amount);
        if overflow {
            anyhow::bail!("Balance debit overflow");
        }

        self.removed = new_total;

        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.added == U256::ZERO && self.removed == U256::ZERO
    }
}

pub type BalanceOverlay = HashMap<Address, OverlayEntry<BalanceDelta>>;

pub(crate) struct StepCtx {
    pub opcode: u8,
    pub pc: u64,
    pub gas_before: u64,
    pub depth: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct TxContext {
    pub typ: String,
    pub from: Address,
    pub to: Address,
    pub input: Bytes,
    pub gas: U256,
    pub value: U256,

    // the fields below are only filled during when the frame is exited
    pub gas_used: Option<U256>,
    pub output: Option<Bytes>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct OverlayCheckpoint {
    pub storage: usize,
    pub code: usize,
    pub balance: usize,
    pub selfdestruct: usize,
}

pub(crate) struct FrameState {
    pub ctx: TxContext,
    pub checkpoint: OverlayCheckpoint,
}

enum OverlayAction<K, V> {
    Inserted(K),
    Updated(K, OverlayEntry<V>),
}

pub(crate) struct OverlayState<K, V> {
    handle: Rc<RefCell<HashMap<K, OverlayEntry<V>>>>,
    journal: RefCell<Vec<OverlayAction<K, V>>>,
}

impl<K, V> OverlayState<K, V>
where
    K: Copy + Eq + Hash,
    V: Clone,
{
    pub(crate) fn new() -> Self {
        Self {
            handle: Rc::new(RefCell::new(HashMap::new())),
            journal: RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn handle(&self) -> Rc<RefCell<HashMap<K, OverlayEntry<V>>>> {
        Rc::clone(&self.handle)
    }

    pub(crate) fn borrow_mut(&self) -> RefMut<'_, HashMap<K, OverlayEntry<V>>> {
        self.handle.borrow_mut()
    }

    pub(crate) fn checkpoint(&self) -> usize {
        self.journal.borrow().len()
    }

    pub(crate) fn record_insert(&self, key: K) {
        self.journal.borrow_mut().push(OverlayAction::Inserted(key));
    }

    pub(crate) fn record_update(&self, key: K, entry: OverlayEntry<V>) {
        self.journal
            .borrow_mut()
            .push(OverlayAction::Updated(key, entry));
    }

    pub(crate) fn revert_to_checkpoint(&self, checkpoint: usize) {
        let mut overlay = self.handle.borrow_mut();
        let mut journal = self.journal.borrow_mut();
        while journal.len() > checkpoint {
            match journal.pop().expect("overlay checkpoint out of bounds") {
                OverlayAction::Inserted(key) => {
                    overlay.remove(&key);
                }
                OverlayAction::Updated(key, entry) => {
                    overlay.insert(key, entry);
                }
            }
        }
    }

    pub(crate) fn clear_journal(&self) {
        self.journal.borrow_mut().clear();
    }

    pub(crate) fn commit(&self) {
        Self::commit_map(&mut self.handle.borrow_mut());
    }

    pub(crate) fn rollback(&self) {
        Self::rollback_map(&mut self.handle.borrow_mut());
    }

    fn commit_map(map: &mut HashMap<K, OverlayEntry<V>>) {
        map.retain(|_, entry| {
            if !entry.committed {
                entry.committed = true;
                entry.previous = None;
            }
            true
        });
    }

    fn rollback_map(map: &mut HashMap<K, OverlayEntry<V>>) {
        map.retain(|_, entry| {
            if entry.committed {
                return true;
            }

            if let Some(prev) = entry.previous.take() {
                entry.value = prev;
                entry.committed = true;
                true
            } else {
                false
            }
        });
    }
}
