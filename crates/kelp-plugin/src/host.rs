use std::collections::HashMap;

use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store, StoreContextMut, StoreLimits, StoreLimitsBuilder};

use crate::{Error, PluginWorld, Result};

mod chunker {
    wasmtime::component::bindgen!({ path: "wit", world: "chunker", additional_derives: [PartialEq, Eq] });
}
mod merger {
    wasmtime::component::bindgen!({ path: "wit", world: "merger", additional_derives: [PartialEq, Eq] });
}
mod differ {
    wasmtime::component::bindgen!({ path: "wit", world: "differ", additional_derives: [PartialEq, Eq] });
}
mod trigger {
    wasmtime::component::bindgen!({ path: "wit", world: "trigger", additional_derives: [PartialEq, Eq] });
}

use chunker::kelp::plugin::types as wit;

pub use wit::{Change, FileVersion, MergeResult, Span, Verdict};

/// Bounds on one plugin call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Units of fuel, roughly instructions, a call may use. Reading a file
    /// costs [`READ_FUEL_PER_BYTE`] per byte as well.
    pub fuel: u64,
    /// Bytes of linear memory a call may grow to. A plugin has at most
    /// [`MAX_MEMORIES`] memories, each bounded by this.
    pub memory: usize,
    /// Bytes of the largest file a trigger may read; larger files read as
    /// none.
    pub read_file: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            fuel: 20_000_000_000,
            memory: 512 << 20,
            read_file: 64 << 20,
        }
    }
}

/// How many linear memories a plugin may have. A component built from one
/// module has one.
pub const MAX_MEMORIES: usize = 1;
/// How many tables, and table elements in all of them, a plugin may have.
const MAX_TABLES: usize = 16;
const MAX_TABLE_ELEMENTS: usize = 100_000;
/// How many core instances a plugin may create.
const MAX_INSTANCES: usize = 16;
/// The fuel a trigger pays for each byte of a file it reads, so that reading
/// files in a loop runs out of fuel like any other loop.
pub const READ_FUEL_PER_BYTE: u64 = 8;

/// Compiles and runs plugins.
#[derive(Clone)]
pub struct Host {
    engine: Engine,
    limits: Limits,
}

/// A compiled plugin.
#[derive(Clone)]
pub struct Plugin {
    component: Component,
    worlds: Vec<PluginWorld>,
}

/// Gives a trigger the new content of a file of the change.
type ReadFile = Box<dyn FnMut(&str) -> Option<Vec<u8>> + Send>;

struct State {
    limits: StoreLimits,
    read_file: Option<ReadFile>,
    /// The largest file the trigger may read.
    read_limit: u64,
    /// The declared size of each file of the change, to refuse large files
    /// before reading them.
    sizes: HashMap<String, u64>,
}

impl State {
    /// Reads a file for the trigger, if it is small enough.
    fn read(&mut self, path: &str) -> Option<Vec<u8>> {
        if self.sizes.get(path).is_some_and(|&s| s > self.read_limit) {
            return None;
        }
        let data = self.read_file.as_mut()?(path)?;
        (data.len() as u64 <= self.read_limit).then_some(data)
    }
}

impl trigger::kelp::plugin::types::Host for State {}

impl trigger::TriggerImports for State {
    fn read_file(&mut self, path: String) -> Option<Vec<u8>> {
        self.read(&path)
    }
}

impl Host {
    /// Creates a host that bounds every call by `limits`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Invalid`] if the compiler cannot be set up for this
    /// machine.
    pub fn new(limits: Limits) -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true).consume_fuel(true);
        let engine = Engine::new(&config).map_err(invalid)?;
        Ok(Self { engine, limits })
    }

    /// Compiles a component and finds out which worlds it implements.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Invalid`] if `wasm` is not a component, implements
    /// none of the worlds, or imports anything but what the trigger world
    /// provides.
    pub fn load(&self, wasm: &[u8]) -> Result<Plugin> {
        let component = Component::new(&self.engine, wasm).map_err(invalid)?;
        let ty = component.component_type();
        let exports: Vec<String> = ty
            .exports(&self.engine)
            .map(|(n, _)| n.to_owned())
            .collect();
        let worlds: Vec<PluginWorld> = PluginWorld::ALL
            .into_iter()
            .filter(|w| exports.iter().any(|e| e == w.export()))
            .collect();
        if worlds.is_empty() {
            return Err(Error::Invalid("it implements no Kelp plugin world".into()));
        }
        for (name, _) in ty.imports(&self.engine) {
            if name != "read-file" && !name.starts_with("kelp:plugin/types") {
                return Err(Error::Invalid(format!(
                    "it imports {name}, which Kelp does not provide"
                )));
            }
        }
        Ok(Plugin { component, worlds })
    }

    fn store(&self, read_file: Option<ReadFile>) -> Result<Store<State>> {
        let state = State {
            limits: StoreLimitsBuilder::new()
                .memory_size(self.limits.memory)
                .memories(MAX_MEMORIES)
                .tables(MAX_TABLES)
                .table_elements(MAX_TABLE_ELEMENTS)
                .instances(MAX_INSTANCES)
                .build(),
            read_file,
            read_limit: self.limits.read_file,
            sizes: HashMap::new(),
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|s| &mut s.limits);
        store.set_fuel(self.limits.fuel).map_err(trap)?;
        Ok(store)
    }
}

impl Plugin {
    /// Returns the worlds the plugin implements.
    pub fn worlds(&self) -> &[PluginWorld] {
        &self.worlds
    }

    fn require(&self, world: PluginWorld) -> Result<()> {
        match self.worlds.contains(&world) {
            true => Ok(()),
            false => Err(Error::WrongWorld(world)),
        }
    }

    /// Splits `data`, the content of the file at `path`, into chunks.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WrongWorld`] if the plugin is not a chunker, or
    /// [`Error::Trap`] if it fails or returns spans that do not cover `data`
    /// exactly, in order.
    pub fn chunk(&self, host: &Host, path: &str, data: &[u8]) -> Result<Vec<Span>> {
        self.require(PluginWorld::Chunker)?;
        let mut store = host.store(None)?;
        let linker = Linker::new(&host.engine);
        let bindings =
            chunker::Chunker::instantiate(&mut store, &self.component, &linker).map_err(trap)?;
        let spans = bindings.call_chunk(&mut store, path, data).map_err(trap)?;
        let mut offset = 0u64;
        for span in &spans {
            if span.offset != offset || span.len == 0 {
                return Err(Error::Trap(
                    "the chunks do not cover the file in order".into(),
                ));
            }
            offset += u64::from(span.len);
        }
        if offset != data.len() as u64 {
            return Err(Error::Trap("the chunks do not cover the file".into()));
        }
        Ok(spans)
    }

    /// Merges two descendants of `base`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WrongWorld`] if the plugin is not a merger, or
    /// [`Error::Trap`] if it fails.
    pub fn merge(
        &self,
        host: &Host,
        base: &FileVersion,
        ours: &FileVersion,
        theirs: &FileVersion,
    ) -> Result<MergeResult> {
        self.require(PluginWorld::Merger)?;
        let mut store = host.store(None)?;
        let linker = Linker::new(&host.engine);
        let bindings =
            merger::Merger::instantiate(&mut store, &self.component, &linker).map_err(trap)?;
        let convert = |v: &FileVersion| merger::kelp::plugin::types::FileVersion {
            path: v.path.clone(),
            content: v.content.clone(),
        };
        let result = bindings
            .call_merge(&mut store, &convert(base), &convert(ours), &convert(theirs))
            .map_err(trap)?;
        Ok(match result {
            merger::kelp::plugin::types::MergeResult::Merged(data) => MergeResult::Merged(data),
            merger::kelp::plugin::types::MergeResult::Conflict(why) => MergeResult::Conflict(why),
        })
    }

    /// Describes the change from `old` to `new` for people.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WrongWorld`] if the plugin is not a differ, or
    /// [`Error::Trap`] if it fails.
    pub fn describe(
        &self,
        host: &Host,
        old: Option<&FileVersion>,
        new: Option<&FileVersion>,
    ) -> Result<String> {
        self.require(PluginWorld::Differ)?;
        let mut store = host.store(None)?;
        let linker = Linker::new(&host.engine);
        let bindings =
            differ::Differ::instantiate(&mut store, &self.component, &linker).map_err(trap)?;
        let convert = |v: &FileVersion| differ::kelp::plugin::types::FileVersion {
            path: v.path.clone(),
            content: v.content.clone(),
        };
        let (old, new) = (old.map(convert), new.map(convert));
        bindings
            .call_describe(&mut store, old.as_ref(), new.as_ref())
            .map_err(trap)
    }

    /// Asks the plugin whether a submit may proceed. `read_file` returns the
    /// new content of a file of the change.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WrongWorld`] if the plugin is not a trigger, or
    /// [`Error::Trap`] if it fails.
    pub fn pre_submit(
        &self,
        host: &Host,
        stream: &str,
        author: &str,
        description: &str,
        changes: &[Change],
        read_file: impl FnMut(&str) -> Option<Vec<u8>> + Send + 'static,
    ) -> Result<Verdict> {
        self.require(PluginWorld::Trigger)?;
        let mut store = host.store(Some(Box::new(read_file)))?;
        store.data_mut().sizes = changes.iter().map(|c| (c.path.clone(), c.size)).collect();
        let mut linker = Linker::new(&host.engine);
        trigger::Trigger::add_to_linker::<State, wasmtime::component::HasSelf<State>>(
            &mut linker,
            |s| s,
        )
        .map_err(invalid)?;
        // The generated import cannot reach the store's fuel; this one
        // charges for the bytes it hands over.
        linker.allow_shadowing(true);
        linker
            .root()
            .func_wrap(
                "read-file",
                |mut store: StoreContextMut<'_, State>, (path,): (String,)| {
                    let data = store.data_mut().read(&path);
                    let cost = data
                        .as_ref()
                        .map_or(0, |d| d.len() as u64)
                        .saturating_mul(READ_FUEL_PER_BYTE);
                    let left = store.get_fuel()?;
                    if cost > left {
                        store.set_fuel(0)?;
                        return Err(wasmtime::Trap::OutOfFuel.into());
                    }
                    store.set_fuel(left - cost)?;
                    Ok((data,))
                },
            )
            .map_err(invalid)?;
        let bindings =
            trigger::Trigger::instantiate(&mut store, &self.component, &linker).map_err(trap)?;
        let changes: Vec<trigger::kelp::plugin::types::Change> = changes
            .iter()
            .map(|c| trigger::kelp::plugin::types::Change {
                path: c.path.clone(),
                action: c.action.clone(),
                size: c.size,
            })
            .collect();
        let verdict = bindings
            .call_pre_submit(&mut store, stream, author, description, &changes)
            .map_err(trap)?;
        Ok(match verdict {
            trigger::kelp::plugin::types::Verdict::Accept => Verdict::Accept,
            trigger::kelp::plugin::types::Verdict::Reject(why) => Verdict::Reject(why),
        })
    }
}

fn invalid(e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("{e:#}"))
}

fn trap(e: impl std::fmt::Display) -> Error {
    Error::Trap(format!("{e:#}"))
}
