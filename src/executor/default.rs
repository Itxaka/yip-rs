//! [`DefaultExecutor`] — production [`Executor`] implementation.
//!
//! Build with [`DefaultExecutor::new`] for the wired-up production
//! executor (every plugin and conditional from upstream yip registered),
//! or with [`DefaultExecutor::empty`] for tests that want to inject a
//! controlled subset.
//!
//! # Examples
//!
//! ```no_run
//! use yip::console::StandardConsole;
//! use yip::executor::{DefaultExecutor, Executor};
//! use yip::vfs::RealVfs;
//!
//! let exec = DefaultExecutor::new();
//! exec.run("rootfs", &RealVfs::new(), &StandardConsole::new(), &[]).unwrap();
//! ```
//!
//! Custom plugin set for a test:
//!
//! ```
//! use std::sync::Arc;
//! use yip::executor::{DefaultExecutor, Plugin};
//!
//! let p: Plugin = Arc::new(|_st, _fs, _con| Ok(()));
//! let exec = DefaultExecutor::empty().with_plugin("noop", p);
//! assert_eq!(exec.plugins.len(), 1);
//! ```
//!
//! # Port notes — differences from `pkg/executor/default.go`
//!
//! 1. **DAG**: yip uses `spectrocloud-labs/herd` for its DAG; we use
//!    `petgraph` + a hand-rolled topological walk. yip doesn't use weak
//!    deps in any meaningful way at the executor layer (only `WeakDeps` is
//!    set as an op option, but every stage op uses it identically), so a
//!    plain topo sort is equivalent.
//!
//! 2. **Conditionals**: Go encodes "skip" as `error != nil` (which is
//!    weird — conditional plugins literally return errors to mean "this
//!    stage shouldn't run"). We use a real tri-state ([`ConditionalOutcome`])
//!    and treat actual errors as `Skip` with a warning, which matches the
//!    observable behaviour.
//!
//! 3. **Modifier**: applied once per raw config blob before parse, as in Go
//!    `schema.Load`. The Go default modifier is `dot_notation_modifier`,
//!    which expands `stages.foo.commands` style keys; the Rust schema
//!    module exposes the same function.
//!
//! 4. **Stage substages** (`rootfs.before` / `rootfs` / `rootfs.after`): in
//!    Go this is done at the call site in `cmd/yip/main.go`, not in the
//!    executor itself. We pull it INTO the executor here because every
//!    caller does it and centralising the loop is less error-prone.
//!    Substages run sequentially; an absent substage is a silent no-op.
//!
//! 5. **stillAlive ticker**: Go spawns a goroutine to log "still running"
//!    every 10s. Skipped — `tracing` spans give us the same observability
//!    without an extra thread per plugin.
//!
//! # Stability
//!
//! Public API. Constructor and builder methods are stable; field layout
//! (`plugins` / `conditionals` / `modifier`) is also public — treat them
//! as read-only after construction.

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use petgraph::graph::{DiGraph, NodeIndex};
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::console::Console;
use crate::error::{Error, Result};
use crate::schema::{Config, Stage};
use crate::vfs::Vfs;

use super::executor::{Conditional, ConditionalOutcome, Executor, Plugin};

/// Default executor with a plugin chain + conditional chain.
///
/// Build with [`DefaultExecutor::new`] for the production wiring (all the
/// upstream yip plugins) or [`DefaultExecutor::empty`] for tests.
///
/// # Examples
///
/// ```
/// use yip::executor::DefaultExecutor;
///
/// let exec = DefaultExecutor::empty();
/// assert_eq!(exec.plugins.len(), 0);
/// assert_eq!(exec.conditionals.len(), 0);
/// ```
pub struct DefaultExecutor {
    /// Registered action plugins, in registration (= execution) order.
    /// Each entry is `(name, callback)`.
    pub plugins: Vec<(String, Plugin)>,
    /// Registered conditionals, in registration order. Evaluated in
    /// sequence; the first `Skip` or error short-circuits the stage.
    pub conditionals: Vec<(String, Conditional)>,
    /// Optional pre-parse modifier (e.g. `dot_notation_modifier`). Applied
    /// to every raw config blob before YAML parsing.
    pub modifier: Option<Arc<dyn Fn(&[u8]) -> Result<Vec<u8>> + Send + Sync>>,
}

impl DefaultExecutor {
    /// Construct an executor with the default plugin + conditional set
    /// (matches Go `NewExecutor()`).
    ///
    /// The defaults include all 7 conditionals and 22 plugins required to
    /// run any upstream yip config. Use [`DefaultExecutor::empty`] +
    /// [`DefaultExecutor::with_plugin`] for tests that need a controlled
    /// subset.
    ///
    /// # Examples
    ///
    /// ```
    /// use yip::executor::DefaultExecutor;
    ///
    /// let exec = DefaultExecutor::new();
    /// assert!(exec.plugins.len() > 0);
    /// assert!(exec.conditionals.len() > 0);
    /// ```
    pub fn new() -> Self {
        Self::empty()
            .with_modifier(Arc::new(|bytes: &[u8]| {
                crate::schema::dot_notation_modifier(bytes)
            }))
            .with_conditional("node", crate::conditionals::node::build())
            .with_conditional("if", crate::conditionals::if_cond::build())
            .with_conditional("only_if_os", crate::conditionals::only_if_os::build())
            .with_conditional("only_if_os_version", crate::conditionals::only_if_os_version::build())
            .with_conditional("if_arch", crate::conditionals::if_arch::build())
            .with_conditional("if_service_manager", crate::conditionals::if_service_manager::build())
            .with_conditional("if_files", crate::conditionals::if_files::build())
            .with_plugin("dns", crate::plugins::dns::build())
            .with_plugin("download", crate::plugins::download::build())
            .with_plugin("git", crate::plugins::git::build())
            .with_plugin("entities", crate::plugins::entities::build())
            .with_plugin("directories", crate::plugins::directories::build())
            .with_plugin("files", crate::plugins::files::build())
            .with_plugin("commands", crate::plugins::commands::build())
            .with_plugin("delete_entities", crate::plugins::entities::build_delete())
            .with_plugin("hostname", crate::plugins::hostname::build())
            .with_plugin("sysctl", crate::plugins::sysctl::build())
            .with_plugin("user", crate::plugins::user::build())
            .with_plugin("ssh", crate::plugins::ssh::build())
            .with_plugin("modules", crate::plugins::modules::build())
            .with_plugin("timesyncd", crate::plugins::timesyncd::build())
            .with_plugin("systemctl", crate::plugins::systemctl::build())
            .with_plugin("environment", crate::plugins::environment::build())
            .with_plugin("systemd_firstboot", crate::plugins::systemd_firstboot::build())
            .with_plugin("datasource", crate::plugins::datasource::build())
            .with_plugin("layout", crate::plugins::layout::build())
            .with_plugin("package_pins", crate::plugins::package_pins::build())
            .with_plugin("packages", crate::plugins::packages::build())
            .with_plugin("unpack_image", crate::plugins::unpack_image::build())
    }

    /// Construct an empty executor; useful for tests that want to inject
    /// specific plugins without the default chain.
    ///
    /// # Examples
    ///
    /// ```
    /// use yip::executor::DefaultExecutor;
    ///
    /// let exec = DefaultExecutor::empty();
    /// assert!(exec.plugins.is_empty());
    /// assert!(exec.modifier.is_none());
    /// ```
    pub fn empty() -> Self {
        Self {
            plugins: Vec::new(),
            conditionals: Vec::new(),
            modifier: None,
        }
    }

    /// Register a plugin. Order matters — plugins run in registration order.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use yip::executor::{DefaultExecutor, Plugin};
    ///
    /// let p: Plugin = Arc::new(|_st, _fs, _con| Ok(()));
    /// let exec = DefaultExecutor::empty().with_plugin("noop", p);
    /// assert_eq!(exec.plugins.len(), 1);
    /// ```
    pub fn with_plugin(mut self, name: &str, p: Plugin) -> Self {
        self.plugins.push((name.to_string(), p));
        self
    }

    /// Register a conditional. Order matters — conditionals run in
    /// registration order until one returns `Skip`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use yip::executor::{Conditional, ConditionalOutcome, DefaultExecutor};
    ///
    /// let c: Conditional = Arc::new(|_st, _fs, _con| Ok(ConditionalOutcome::Run));
    /// let exec = DefaultExecutor::empty().with_conditional("always", c);
    /// assert_eq!(exec.conditionals.len(), 1);
    /// ```
    pub fn with_conditional(mut self, name: &str, c: Conditional) -> Self {
        self.conditionals.push((name.to_string(), c));
        self
    }

    /// Replace the pre-parse modifier.
    ///
    /// The modifier runs on the raw bytes of every config blob *after*
    /// template rendering and *before* YAML parsing. The default impl
    /// (installed by [`DefaultExecutor::new`]) is the
    /// `dot_notation_modifier`, which expands `stages.foo.commands`-style
    /// dotted keys into nested maps.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use yip::executor::DefaultExecutor;
    ///
    /// let exec = DefaultExecutor::empty()
    ///     .with_modifier(Arc::new(|b| Ok(b.to_vec())));
    /// assert!(exec.modifier.is_some());
    /// ```
    pub fn with_modifier(
        mut self,
        m: Arc<dyn Fn(&[u8]) -> Result<Vec<u8>> + Send + Sync>,
    ) -> Self {
        self.modifier = Some(m);
        self
    }

    /// Run conditionals for a stage. Returns `false` if any conditional
    /// said `Skip` (or errored). Matches Go's `applyStage` preflight loop.
    fn check_conditionals(
        &self,
        stage_name: &str,
        stage: &Stage,
        fs: &dyn Vfs,
        console: &dyn Console,
    ) -> bool {
        for (cname, c) in &self.conditionals {
            match c(stage, fs, console) {
                Ok(ConditionalOutcome::Skip) => {
                    info!(stage = stage_name, conditional = cname.as_str(), "step skipped");
                    return false;
                }
                Ok(ConditionalOutcome::Run) => {}
                Err(e) => {
                    // Go: conditional errors are warned and treated as skip.
                    warn!(
                        stage = stage_name,
                        conditional = cname.as_str(),
                        error = %e,
                        "conditional errored, skipping stage"
                    );
                    return false;
                }
            }
        }
        true
    }

    /// Run the plugin chain for a stage. Accumulates errors into `errs`;
    /// never aborts. Matches Go `applyStage`'s plugin loop.
    fn run_plugins(
        &self,
        stage_name: &str,
        stage: &Stage,
        fs: &dyn Vfs,
        console: &dyn Console,
        errs: &mut Vec<Error>,
    ) {
        debug!(stage = stage_name, "running plugin chain");
        for (pname, p) in &self.plugins {
            if let Err(e) = p(stage, fs, console) {
                warn!(stage = stage_name, plugin = pname.as_str(), error = %e, "plugin failed");
                errs.push(Error::Plugin {
                    plugin: pname.clone(),
                    source: Box::new(e),
                });
            }
        }
    }

    /// Apply one stage (conditionals + plugin chain). Mirrors Go `applyStage`.
    fn apply_stage(
        &self,
        stage_name: &str,
        stage: &Stage,
        fs: &dyn Vfs,
        console: &dyn Console,
    ) -> Vec<Error> {
        let mut errs = Vec::new();
        if !self.check_conditionals(stage_name, stage, fs, console) {
            return errs;
        }
        info!(
            stage = stage_name,
            commands = stage_command_count(stage),
            files = stage_file_count(stage),
            "processing stage"
        );
        self.run_plugins(stage_name, stage, fs, console, &mut errs);
        errs
    }

    /// Resolve one user-supplied path/URL/inline string into one or more
    /// `(source_label, Config)` pairs. Mirrors the `switch` in Go
    /// `prepareDAG`.
    fn resolve_source(&self, source: &str) -> Result<Vec<(String, Config)>> {
        // stdin sentinel
        if source == "-" {
            let mut buf = Vec::new();
            std::io::stdin().read_to_end(&mut buf).map_err(Error::io)?;
            let cfg = self.parse_bytes(&buf)?;
            return Ok(vec![("<STDIN>".to_string(), cfg)]);
        }

        // URL
        if is_url(source) {
            let bytes = fetch_url(source)?;
            let cfg = self.parse_bytes(&bytes)?;
            return Ok(vec![(source.to_string(), cfg)]);
        }

        // Filesystem: file or dir
        let p = Path::new(source);
        if p.exists() {
            let md = fs::metadata(p).map_err(|e| Error::io_at(p, e))?;
            if md.is_dir() {
                return self.load_dir(p);
            }
            let bytes = fs::read(p).map_err(|e| Error::io_at(p, e))?;
            let cfg = self.parse_bytes(&bytes)?;
            return Ok(vec![(source.to_string(), cfg)]);
        }

        // Looks like a filesystem path but doesn't exist? Silently skip
        // with a debug trace — matches Go yip's behaviour where missing
        // cloud-init dirs (e.g. `/usr/local/cloud-config/` on a livecd
        // before sysroot is populated) are no-ops, not errors.
        if looks_like_fs_path(source) {
            debug!(source, "resolve_source: filesystem path absent, skipping");
            return Ok(Vec::new());
        }

        // Inline YAML heuristic — matches Go's fallback (`schema.Load` with
        // nil source-type, which parses the string itself).
        if looks_like_inline_yaml(source) {
            let cfg = self.parse_bytes(source.as_bytes())?;
            return Ok(vec![("<INLINE>".to_string(), cfg)]);
        }

        Err(Error::other(format!(
            "could not resolve source {source:?}: not a file, dir, url, or inline yaml"
        )))
    }

    /// Walk a directory for `*.yaml` / `*.yml`, sorted lexicographically.
    fn load_dir(&self, dir: &Path) -> Result<Vec<(String, Config)>> {
        let mut entries: Vec<PathBuf> = WalkDir::new(dir)
            .sort_by_file_name()
            .into_iter()
            .filter_map(|r| r.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.into_path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|s| s.to_str()),
                    Some("yaml") | Some("yml")
                )
            })
            .collect();
        entries.sort();

        let mut out = Vec::with_capacity(entries.len());
        for p in entries {
            let bytes = fs::read(&p).map_err(|e| Error::io_at(&p, e))?;
            let cfg = self.parse_bytes(&bytes)?;
            out.push((p.display().to_string(), cfg));
        }
        Ok(out)
    }

    /// Apply modifier (if any) + template render + YAML parse.
    fn parse_bytes(&self, bytes: &[u8]) -> Result<Config> {
        // Templating preprocess: inject system facts (OS, hostname, UUID).
        // Errors are swallowed (warn + fall back to raw) to match Go's
        // `templateSysData`, which discards `TemplatedString` failures and
        // continues with the original bytes.
        let mut rendered = render_template(bytes);

        // Apply user modifier (e.g. dot_notation_modifier) AFTER template
        // render, matching the order in Go schema.Load: template first
        // (`templateSysData`), then modifier.
        if let Some(m) = &self.modifier {
            rendered = m(&rendered)?;
        }

        Config::load(&rendered)
    }

    /// Build the DAG for a single (source, config) pair and return the
    /// topologically-ordered list of (op_name, stage_clone) pairs. Mirrors
    /// Go `genOpFromSchema` + `uniqueNames` + `g.Analyze()` flattened.
    fn build_dag_for_config(
        &self,
        source: &str,
        stage_key: &str,
        cfg: &Config,
    ) -> Result<Vec<(String, Stage)>> {
        let stages = stages_for(cfg, stage_key);
        if stages.is_empty() {
            return Ok(Vec::new());
        }

        // Detect duplicate stage names — if any, suffix every name with its
        // index to disambiguate (matches Go `checkDuplicates`).
        let dup = has_duplicate_names(&stages);

        let rootname = if !cfg_name(cfg).is_empty() {
            cfg_name(cfg)
        } else {
            source.to_string()
        };

        let mut graph: DiGraph<(String, Stage), ()> = DiGraph::new();
        let mut order: Vec<NodeIndex> = Vec::with_capacity(stages.len());

        // Pass 1: add all nodes.
        for (i, st) in stages.iter().enumerate() {
            let raw_name = stage_name(st);
            let mut name = if dup {
                format!("{}.{}", raw_name, i)
            } else {
                raw_name.to_string()
            };
            if name.is_empty() {
                name = i.to_string();
            }
            let op_name = format!("{rootname}.{name}");
            let idx = graph.add_node((op_name, st.clone()));
            order.push(idx);
        }

        // Pass 2: wire deps. We do this after all nodes exist so that
        // `after: [a]` from stage `b` resolves whether `a` is declared
        // before OR after `b` in the YAML.
        let mut prev: Option<NodeIndex> = None;
        for (i, st) in stages.iter().enumerate() {
            let idx = order[i];
            let afters = stage_afters(st);
            let had_explicit_after = !afters.is_empty();

            for dep_name in &afters {
                // Multiple matches → depend on all of them. Search the
                // full stage list (both directions).
                for (j, other) in stages.iter().enumerate() {
                    if j != i && stage_name(other) == dep_name.as_str() {
                        graph.add_edge(order[j], idx, ());
                    }
                }
            }

            // Implicit lexical chain only when no explicit after: deps
            // were given. Matches Go's `prev` wiring. Stages with explicit
            // after also don't become `prev` themselves — otherwise
            // `b after: [a]` followed by `a` would build edges in BOTH
            // directions (a→b from after, b→a from lexical) and cycle.
            if !had_explicit_after {
                if let Some(prev_idx) = prev {
                    graph.add_edge(prev_idx, idx, ());
                }
                prev = Some(idx);
            }
        }

        // Topological sort. Cycles → error (Go's herd would also fail).
        let topo = petgraph::algo::toposort(&graph, None).map_err(|cyc| {
            Error::other(format!(
                "cycle detected in stage dependencies at node {:?}",
                graph.node_weight(cyc.node_id())
                    .map(|(n, _)| n.as_str())
                    .unwrap_or("<unknown>")
            ))
        })?;

        Ok(topo
            .into_iter()
            .map(|idx| graph[idx].clone())
            .collect())
    }

    /// Execute one source for one stage. Mirrors Go `runStage`.
    fn run_stage_for_source(
        &self,
        stage_key: &str,
        source: &str,
        cfg: &Config,
        fs: &dyn Vfs,
        console: &dyn Console,
    ) -> Vec<Error> {
        let mut errs = Vec::new();
        let ordered = match self.build_dag_for_config(source, stage_key, cfg) {
            Ok(o) => o,
            Err(e) => {
                errs.push(e);
                return errs;
            }
        };

        for (op_name, stage) in ordered {
            debug!(op = op_name.as_str(), "executing stage op");
            let mut stage_errs = self.apply_stage(&op_name, &stage, fs, console);
            errs.append(&mut stage_errs);
        }
        errs
    }
}

impl Default for DefaultExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl Executor for DefaultExecutor {
    fn run(
        &self,
        stage: &str,
        fs: &dyn Vfs,
        console: &dyn Console,
        paths: &[String],
    ) -> Result<()> {
        info!(stage = stage, "running stage");

        // Substage expansion: for stage X, run X.before, X, X.after in that
        // order. Matches the call-site loop in Go `cmd/yip/main.go`.
        let stage_keys = substages_for(stage);

        let mut errs: Vec<Error> = Vec::new();
        for source in paths {
            let resolved = match self.resolve_source(source) {
                Ok(r) => r,
                Err(e) => {
                    warn!(source = source.as_str(), error = %e, "failed to resolve source");
                    errs.push(e);
                    continue;
                }
            };

            for (label, cfg) in resolved {
                for skey in &stage_keys {
                    let mut e = self.run_stage_for_source(skey, &label, &cfg, fs, console);
                    errs.append(&mut e);
                }
            }
        }

        info!(stage = stage, "done executing stage");
        finish(errs)
    }

    fn apply(
        &self,
        stage: &str,
        cfg: &Config,
        fs: &dyn Vfs,
        console: &dyn Console,
    ) -> Result<()> {
        let stage_keys = substages_for(stage);
        let mut errs: Vec<Error> = Vec::new();

        for skey in &stage_keys {
            let stages = stages_for(cfg, skey);
            if stages.is_empty() {
                debug!(stage = skey.as_str(), "no stages defined, skipping");
                continue;
            }
            info!(
                stage = skey.as_str(),
                total = stages.len(),
                "applying stage"
            );

            // Apply doesn't build a DAG — Go's `Apply` iterates stages in
            // declaration order. We match that.
            for (i, st) in stages.iter().enumerate() {
                let name = {
                    let raw = stage_name(st);
                    if raw.is_empty() {
                        i.to_string()
                    } else {
                        raw.to_string()
                    }
                };
                let mut e = self.apply_stage(&name, st, fs, console);
                errs.append(&mut e);
            }
        }

        finish(errs)
    }

    fn analyze(&self, stage: &str, cfg: &Config) -> Vec<String> {
        let mut out = Vec::new();
        for skey in substages_for(stage) {
            if let Ok(ordered) = self.build_dag_for_config("<analyze>", &skey, cfg) {
                for (name, _) in ordered {
                    out.push(name);
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Combine accumulated errors into a single `Result`. Empty → Ok.
/// Single → that error. Multiple → `Error::Multi`.
fn finish(mut errs: Vec<Error>) -> Result<()> {
    match errs.len() {
        0 => Ok(()),
        1 => Err(errs.pop().unwrap()),
        _ => Err(Error::Multi(errs)),
    }
}

/// Expand `"rootfs"` → `["rootfs.before", "rootfs", "rootfs.after"]`.
/// Empty stage names pass through unchanged so callers can opt out.
fn substages_for(stage: &str) -> Vec<String> {
    if stage.is_empty() {
        return vec![String::new()];
    }
    // Heuristic: only expand if the stage isn't already a substage.
    // Otherwise running `rootfs.before` would expand to
    // `rootfs.before.before` which is nonsense.
    if stage.ends_with(".before") || stage.ends_with(".after") {
        return vec![stage.to_string()];
    }
    vec![
        format!("{stage}.before"),
        stage.to_string(),
        format!("{stage}.after"),
    ]
}

fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

fn looks_like_inline_yaml(s: &str) -> bool {
    s.contains(':') && s.contains('\n')
}

/// True when `s` is shaped like a filesystem path (absolute, relative-with-`/`,
/// or trailing slash) so missing-file callers can silent-skip instead of
/// erroring as "not a file/dir/url/inline".
fn looks_like_fs_path(s: &str) -> bool {
    s.starts_with('/')
        || s.starts_with("./")
        || s.starts_with("../")
        || s.ends_with('/')
        || (s.contains('/') && !s.contains(':'))
}

fn fetch_url(url: &str) -> Result<Vec<u8>> {
    let resp = reqwest::blocking::get(url)
        .map_err(|e| Error::other(format!("http get {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::other(format!(
            "http get {url}: status {}",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .map_err(|e| Error::other(format!("http body {url}: {e}")))?;
    Ok(bytes.to_vec())
}

/// Template-render a raw config blob through `crate::template::render_with_sysdata`.
///
/// Failure modes match Go's `templateSysData`: any error (invalid UTF-8,
/// bad template syntax, sysdata gather failure) is logged as a warning and
/// the original raw bytes are returned unchanged. Callers must not treat
/// templating failure as a hard error — the upstream Go implementation
/// swallows these silently.
fn render_template(bytes: &[u8]) -> Vec<u8> {
    let template_str = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "config bytes are not valid UTF-8, skipping template render");
            return bytes.to_vec();
        }
    };

    match crate::template::render_with_sysdata(template_str) {
        Ok(rendered) => rendered.into_bytes(),
        Err(e) => {
            warn!(error = %e, "template render failed, falling back to raw bytes");
            bytes.to_vec()
        }
    }
}

// ---------------------------------------------------------------------------
// schema shims
// ---------------------------------------------------------------------------
//
// The executor needs five things from the schema crate:
//   * `cfg.name` (for op naming)
//   * `cfg.stages[stage_key]` (a Vec<Stage>)
//   * `stage.name` (for op naming + duplicate detection)
//   * `stage.after` (Vec of stage references with a `.name` field)
//   * `stage.commands.len()` / `stage.files.len()` (for the info log)
//
// We access them via direct field paths matching the Go schema layout
// (`Name`, `Stages`, `After`, `Commands`, `Files`). The wave-1 schema agent
// will land the actual struct definitions; if they pick different field
// names you change them HERE and the rest of the module stays untouched.

fn stages_for<'a>(cfg: &'a Config, key: &str) -> &'a [Stage] {
    cfg.stages
        .get(key)
        .map(|v| v.as_slice())
        .unwrap_or(&[])
}

fn cfg_name(cfg: &Config) -> String {
    cfg.name.clone()
}

fn stage_name(st: &Stage) -> &str {
    st.name.as_str()
}

fn stage_afters(st: &Stage) -> Vec<String> {
    st.after.iter().map(|d| d.name.clone()).collect()
}

fn stage_command_count(st: &Stage) -> usize {
    st.commands.len()
}

fn stage_file_count(st: &Stage) -> usize {
    st.files.len()
}

fn has_duplicate_names(stages: &[Stage]) -> bool {
    let mut seen: HashSet<&str> = HashSet::new();
    for s in stages {
        let n = stage_name(s);
        if n.is_empty() {
            continue;
        }
        if !seen.insert(n) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    // ---- Test doubles ------------------------------------------------------

    struct NullConsole;
    impl Console for NullConsole {
        fn run(&self, _cmd: &str) -> Result<String> {
            Ok(String::new())
        }
        fn run_in(&self, _cwd: &Path, _cmd: &str) -> Result<String> {
            Ok(String::new())
        }
    }

    /// Use the wave-1 RealVfs for tests that exercise on-disk paths.
    use crate::vfs::RealVfs;
    type DiskVfs = RealVfs;

    fn counter_plugin(counter: Arc<AtomicUsize>) -> Plugin {
        Arc::new(move |_stage, _fs, _con| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn recording_plugin(log: Arc<Mutex<Vec<String>>>, label: &'static str) -> Plugin {
        Arc::new(move |stage: &Stage, _fs, _con| {
            let name = stage.name.clone();
            log.lock().unwrap().push(format!("{label}:{name}"));
            Ok(())
        })
    }

    fn failing_plugin(name: &'static str) -> Plugin {
        Arc::new(move |_stage, _fs, _con| Err(Error::other(format!("boom-{name}"))))
    }

    fn const_conditional(out: ConditionalOutcome) -> Conditional {
        Arc::new(move |_stage, _fs, _con| Ok(out))
    }

    /// Build a minimal Config from a YAML string. Tests assume the schema
    /// module exposes `Config::load`.
    fn cfg(yaml: &str) -> Config {
        Config::load(yaml.as_bytes()).expect("test yaml parses")
    }

    // ---- Tests -------------------------------------------------------------

    #[test]
    fn empty_config_is_noop() {
        let exec = DefaultExecutor::empty();
        let c = cfg("name: empty\nstages: {}\n");
        let res = exec.apply("rootfs", &c, &RealVfs::new(), &NullConsole);
        assert!(res.is_ok(), "empty config should be Ok: {res:?}");
    }

    const ONE_STAGE_YAML: &str = r#"
name: t
stages:
  rootfs:
    - name: one
"#;

    const TWO_STAGE_YAML: &str = r#"
name: t
stages:
  rootfs:
    - name: a
    - name: b
"#;

    const SUBSTAGE_YAML: &str = r#"
name: t
stages:
  rootfs.before:
    - name: a
  rootfs:
    - name: b
  rootfs.after:
    - name: c
"#;

    #[test]
    fn single_plugin_runs_once_per_stage() {
        let counter = Arc::new(AtomicUsize::new(0));
        let exec = DefaultExecutor::empty()
            .with_plugin("count", counter_plugin(counter.clone()));

        let c = cfg(ONE_STAGE_YAML);
        exec.apply("rootfs", &c, &RealVfs::new(), &NullConsole).unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn conditional_skip_prevents_plugins() {
        let counter = Arc::new(AtomicUsize::new(0));
        let exec = DefaultExecutor::empty()
            .with_conditional("never", const_conditional(ConditionalOutcome::Skip))
            .with_plugin("count", counter_plugin(counter.clone()));

        let c = cfg(ONE_STAGE_YAML);
        exec.apply("rootfs", &c, &RealVfs::new(), &NullConsole).unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 0, "skip should suppress plugin");
    }

    #[test]
    fn substages_run_in_order() {
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let exec = DefaultExecutor::empty()
            .with_plugin("log", recording_plugin(log.clone(), "p"));

        let c = cfg(SUBSTAGE_YAML);
        exec.apply("rootfs", &c, &RealVfs::new(), &NullConsole).unwrap();

        let l = log.lock().unwrap();
        assert_eq!(*l, vec!["p:a".to_string(), "p:b".to_string(), "p:c".to_string()]);
    }

    #[test]
    fn plugin_errors_are_aggregated_not_aborted() {
        let counter = Arc::new(AtomicUsize::new(0));
        let exec = DefaultExecutor::empty()
            .with_plugin("first", failing_plugin("first"))
            .with_plugin("count", counter_plugin(counter.clone()))
            .with_plugin("second", failing_plugin("second"));

        let c = cfg(ONE_STAGE_YAML);
        let res = exec.apply("rootfs", &c, &RealVfs::new(), &NullConsole);
        assert!(res.is_err());
        match res.unwrap_err() {
            Error::Multi(v) => assert_eq!(v.len(), 2, "expected both plugin errors"),
            other => panic!("expected Multi, got {other:?}"),
        }
        // The middle plugin still ran — Go's multierror.Append doesn't short-circuit.
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn analyze_returns_op_names_without_side_effects() {
        let counter = Arc::new(AtomicUsize::new(0));
        let exec = DefaultExecutor::empty()
            .with_plugin("count", counter_plugin(counter.clone()));

        let c = cfg(TWO_STAGE_YAML);
        let names = exec.analyze("rootfs", &c);
        assert!(names.iter().any(|n| n.ends_with(".a")), "got {names:?}");
        assert!(names.iter().any(|n| n.ends_with(".b")), "got {names:?}");
        assert_eq!(counter.load(Ordering::SeqCst), 0, "analyze must not invoke plugins");
    }

    #[test]
    fn directory_walk_picks_up_yaml_and_yml_only() {
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let exec = DefaultExecutor::empty()
            .with_plugin("log", recording_plugin(log.clone(), "p"));

        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.yaml"),
            "name: A\nstages:\n  rootfs:\n    - name: from_yaml\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("b.yml"),
            "name: B\nstages:\n  rootfs:\n    - name: from_yml\n",
        )
        .unwrap();
        fs::write(dir.path().join("c.txt"), "ignored").unwrap();
        fs::write(dir.path().join("README.md"), "ignored").unwrap();

        let path_str = dir.path().display().to_string();
        exec.run("rootfs", &RealVfs::new(), &NullConsole, &[path_str]).unwrap();

        let l = log.lock().unwrap();
        // Each .yaml/.yml is loaded; rootfs.before / rootfs.after are absent so
        // only the `rootfs` substage runs. Each file contributes one stage.
        assert!(l.iter().any(|s| s.contains("from_yaml")), "got {l:?}");
        assert!(l.iter().any(|s| s.contains("from_yml")), "got {l:?}");
        assert_eq!(l.len(), 2, "non-yaml files must be ignored: {l:?}");
    }

    #[test]
    fn after_dependency_runs_in_topological_order() {
        // `b` declares `after: [{name: a}]`. Even though `b` appears first in
        // the YAML, the DAG must run `a` first. We exercise the DAG path via
        // `run()` (Go's `Apply` does NOT build a DAG — it iterates in
        // declaration order — so we have to go through `run`).
        let yaml = r#"
name: t
stages:
  rootfs:
    - name: b
      after:
        - name: a
    - name: a
"#;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("c.yaml");
        fs::write(&f, yaml).unwrap();

        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let exec = DefaultExecutor::empty()
            .with_plugin("log", recording_plugin(log.clone(), "p"));
        exec.run("rootfs", &RealVfs::new(), &NullConsole, &[f.display().to_string()])
            .unwrap();

        let l = log.lock().unwrap();
        let pos_a = l.iter().position(|s| s.ends_with(":a")).expect("a ran");
        let pos_b = l.iter().position(|s| s.ends_with(":b")).expect("b ran");
        assert!(pos_a < pos_b, "a must run before b: {l:?}");
    }

    #[test]
    fn default_executor_registers_all_plugins() {
        let exec = DefaultExecutor::new();
        // 22 plugins: entities + delete_entities + the other 20.
        // (Go has 23 because of UnpackImage having both enabled/disabled
        // build variants — yip-rs collapses them under a feature flag.)
        assert_eq!(exec.plugins.len(), 22, "expected 22 plugins, got {}", exec.plugins.len());
        // Spot-check a few key names.
        let names: Vec<&str> = exec.plugins.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"dns"));
        assert!(names.contains(&"files"));
        assert!(names.contains(&"users") || names.contains(&"user"));
        assert!(names.contains(&"layout"));
    }

    #[test]
    fn default_executor_registers_all_conditionals() {
        let exec = DefaultExecutor::new();
        assert_eq!(exec.conditionals.len(), 7, "expected 7 conditionals, got {}", exec.conditionals.len());
    }

    // -----------------------------------------------------------------------
    // Ported from Go `pkg/executor/default_test.go` — additional cases.
    // -----------------------------------------------------------------------

    /// Mirrors Go: `rootfs.before` runs in its own substage; iterating `rootfs`
    /// should run before, main, then after — in that order.
    #[test]
    fn go_multiple_instructions_substages_ordered() {
        let yaml = r#"
name: "Rootfs Layout Settings"
stages:
  rootfs.before:
    - name: "before rootfs"
  rootfs:
    - name: "rootfs"
    - name: "rootfs 2"
  initramfs:
    - name: "initramfs"
"#;
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let exec = DefaultExecutor::empty()
            .with_plugin("log", recording_plugin(log.clone(), "p"));
        let c = cfg(yaml);
        exec.apply("rootfs", &c, &RealVfs::new(), &NullConsole).unwrap();
        let l = log.lock().unwrap();
        assert_eq!(
            *l,
            vec![
                "p:before rootfs".to_string(),
                "p:rootfs".to_string(),
                "p:rootfs 2".to_string(),
            ]
        );
    }

    /// Mirrors Go: 4 unnamed steps in the same stage — they should NOT merge.
    #[test]
    fn go_unnamed_steps_in_sequence_do_not_merge() {
        let yaml = r#"
stages:
  initramfs:
    - commands: ["a"]
    - commands: ["b"]
    - commands: ["c"]
    - commands: ["d"]
"#;
        let counter = Arc::new(AtomicUsize::new(0));
        let exec = DefaultExecutor::empty()
            .with_plugin("count", counter_plugin(counter.clone()));
        let c = cfg(yaml);
        exec.apply("initramfs", &c, &RealVfs::new(), &NullConsole).unwrap();
        // 4 stages → 4 plugin invocations.
        assert_eq!(counter.load(Ordering::SeqCst), 4);
    }

    /// Mirrors Go: duplicate names must not collapse to one op; the executor
    /// disambiguates them in the DAG.
    #[test]
    fn go_duplicate_named_stages_do_not_merge() {
        let yaml = r#"
stages:
  initramfs:
    - name: "Create Kairos User"
      commands: ["a"]
    - commands: ["b"]
    - name: "Create Kairos User"
      commands: ["c"]
    - commands: ["d"]
"#;
        let counter = Arc::new(AtomicUsize::new(0));
        let exec = DefaultExecutor::empty()
            .with_plugin("count", counter_plugin(counter.clone()));
        let c = cfg(yaml);
        // Apply runs in declaration order (no DAG); we just need 4 invocations.
        exec.apply("initramfs", &c, &RealVfs::new(), &NullConsole).unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 4);
    }

    /// Mirrors Go "has multiple instructions in different files": analyze a
    /// directory and verify the ordered op-name list.
    #[test]
    fn go_multiple_files_aggregate_in_order() {
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let exec = DefaultExecutor::empty()
            .with_plugin("log", recording_plugin(log.clone(), "p"));

        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("01_first.yaml"),
            r#"
name: "Rootfs Layout Settings"
stages:
  rootfs.before:
    - name: "before roots"
  rootfs:
    - name: "rootfs"
    - name: "rootfs 2"
  initramfs:
    - name: "initramfs"
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("02_second.yaml"),
            r#"
name: "second Rootfs Layout Settings"
stages:
  rootfs.before:
    - name: "second before roots"
  rootfs:
    - name: "second rootfs"
    - name: "second rootfs 2"
  initramfs:
    - name: "second initramfs"
"#,
        )
        .unwrap();

        let path_str = dir.path().display().to_string();
        exec.run("rootfs", &RealVfs::new(), &NullConsole, &[path_str])
            .unwrap();

        // For substage `rootfs`, the expected ordering pulls the .before
        // entries first, then the main rootfs stages.
        let l = log.lock().unwrap();
        let pos = |needle: &str| l.iter().position(|s| s.ends_with(needle));
        // Files are walked lexicographically; within a file, .before runs
        // before the main stage.
        assert!(pos("before roots").unwrap() < pos("rootfs").unwrap());
        assert!(pos("second before roots").unwrap() < pos("second rootfs").unwrap());
        // 01_first comes before 02_second.
        assert!(pos("rootfs").unwrap() < pos("second rootfs").unwrap());
    }

    /// Mirrors Go "Skip with if conditionals" — but at the plugin level we
    /// only test that an empty `if` doesn't break the chain. The conditional
    /// runtime is covered elsewhere; here we just exercise the apply loop.
    #[test]
    fn empty_if_string_does_not_skip() {
        let yaml = r#"
stages:
  rootfs:
    - name: a
      if: ""
"#;
        let counter = Arc::new(AtomicUsize::new(0));
        let exec = DefaultExecutor::empty()
            .with_plugin("count", counter_plugin(counter.clone()));
        let c = cfg(yaml);
        exec.apply("rootfs", &c, &RealVfs::new(), &NullConsole).unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    /// Conditional that returns Run → plugins run.
    #[test]
    fn conditional_run_allows_plugins() {
        let counter = Arc::new(AtomicUsize::new(0));
        let exec = DefaultExecutor::empty()
            .with_conditional("always", const_conditional(ConditionalOutcome::Run))
            .with_plugin("count", counter_plugin(counter.clone()));
        let c = cfg(ONE_STAGE_YAML);
        exec.apply("rootfs", &c, &RealVfs::new(), &NullConsole).unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    /// Conditional that errors → stage is skipped (same as `Skip` outcome).
    #[test]
    fn conditional_error_is_treated_as_skip() {
        let counter = Arc::new(AtomicUsize::new(0));
        let erroring: Conditional =
            Arc::new(|_s, _f, _c| Err(Error::other("conditional boom")));
        let exec = DefaultExecutor::empty()
            .with_conditional("bad", erroring)
            .with_plugin("count", counter_plugin(counter.clone()));
        let c = cfg(ONE_STAGE_YAML);
        let res = exec.apply("rootfs", &c, &RealVfs::new(), &NullConsole);
        // Conditional errors do not propagate; the stage is just skipped.
        assert!(res.is_ok(), "expected ok, got {res:?}");
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    /// Multiple conditionals: any Skip → no plugins. Order matters — we stop
    /// at the first Skip, but the final observable behaviour is the same.
    #[test]
    fn first_skip_stops_conditional_chain() {
        let after_called = Arc::new(AtomicUsize::new(0));
        let after_called_clone = after_called.clone();
        let after: Conditional = Arc::new(move |_s, _f, _c| {
            after_called_clone.fetch_add(1, Ordering::SeqCst);
            Ok(ConditionalOutcome::Run)
        });
        let counter = Arc::new(AtomicUsize::new(0));
        let exec = DefaultExecutor::empty()
            .with_conditional("skip", const_conditional(ConditionalOutcome::Skip))
            .with_conditional("after", after)
            .with_plugin("count", counter_plugin(counter.clone()));
        let c = cfg(ONE_STAGE_YAML);
        exec.apply("rootfs", &c, &RealVfs::new(), &NullConsole).unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert_eq!(
            after_called.load(Ordering::SeqCst),
            0,
            "after-Skip conditional must not be invoked"
        );
    }

    /// Multierror with 3+ errors aggregates correctly.
    #[test]
    fn multierror_aggregates_three_errors() {
        let exec = DefaultExecutor::empty()
            .with_plugin("a", failing_plugin("a"))
            .with_plugin("b", failing_plugin("b"))
            .with_plugin("c", failing_plugin("c"));
        let c = cfg(ONE_STAGE_YAML);
        let err = exec
            .apply("rootfs", &c, &RealVfs::new(), &NullConsole)
            .unwrap_err();
        match err {
            Error::Multi(v) => assert_eq!(v.len(), 3),
            other => panic!("expected Multi, got {other:?}"),
        }
    }

    /// Multierror with errors across multiple stages aggregates across stages.
    #[test]
    fn multierror_aggregates_across_stages() {
        let yaml = r#"
stages:
  rootfs:
    - name: s1
    - name: s2
"#;
        let exec = DefaultExecutor::empty()
            .with_plugin("fail", failing_plugin("oops"));
        let c = cfg(yaml);
        let err = exec
            .apply("rootfs", &c, &RealVfs::new(), &NullConsole)
            .unwrap_err();
        match err {
            Error::Multi(v) => assert_eq!(v.len(), 2),
            other => panic!("expected Multi, got {other:?}"),
        }
    }

    /// Single error is returned bare (not wrapped in Multi).
    #[test]
    fn single_error_is_not_wrapped_in_multi() {
        let exec = DefaultExecutor::empty().with_plugin("fail", failing_plugin("oops"));
        let c = cfg(ONE_STAGE_YAML);
        let err = exec
            .apply("rootfs", &c, &RealVfs::new(), &NullConsole)
            .unwrap_err();
        // The error is wrapped in Plugin (since run_plugins wraps each).
        match err {
            Error::Plugin { .. } => {}
            other => panic!("expected Plugin, got {other:?}"),
        }
    }

    /// `after:` deps: long chain a→b→c→d (b after a, c after b, d after c).
    #[test]
    fn after_long_chain_runs_in_order() {
        let yaml = r#"
stages:
  rootfs:
    - name: d
      after: [{name: c}]
    - name: b
      after: [{name: a}]
    - name: c
      after: [{name: b}]
    - name: a
"#;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("c.yaml");
        fs::write(&f, yaml).unwrap();

        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let exec = DefaultExecutor::empty().with_plugin("log", recording_plugin(log.clone(), "p"));
        exec.run("rootfs", &RealVfs::new(), &NullConsole, &[f.display().to_string()])
            .unwrap();
        let l = log.lock().unwrap();
        let pos = |n: &str| l.iter().position(|s| s.ends_with(n)).expect("ran");
        assert!(pos(":a") < pos(":b"));
        assert!(pos(":b") < pos(":c"));
        assert!(pos(":c") < pos(":d"));
    }

    /// `after:` deps: diamond a→{b,c}→d.
    #[test]
    fn after_diamond_runs_in_topo_order() {
        let yaml = r#"
stages:
  rootfs:
    - name: d
      after:
        - name: b
        - name: c
    - name: b
      after: [{name: a}]
    - name: c
      after: [{name: a}]
    - name: a
"#;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("c.yaml");
        fs::write(&f, yaml).unwrap();

        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let exec = DefaultExecutor::empty().with_plugin("log", recording_plugin(log.clone(), "p"));
        exec.run("rootfs", &RealVfs::new(), &NullConsole, &[f.display().to_string()])
            .unwrap();
        let l = log.lock().unwrap();
        let pos = |n: &str| l.iter().position(|s| s.ends_with(n)).expect("ran");
        assert!(pos(":a") < pos(":b"));
        assert!(pos(":a") < pos(":c"));
        assert!(pos(":b") < pos(":d"));
        assert!(pos(":c") < pos(":d"));
    }

    /// `after:` deps: cycle detection — a→b, b→a should error.
    #[test]
    fn after_cycle_is_detected() {
        let yaml = r#"
stages:
  rootfs:
    - name: a
      after: [{name: b}]
    - name: b
      after: [{name: a}]
"#;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("c.yaml");
        fs::write(&f, yaml).unwrap();

        let exec = DefaultExecutor::empty();
        let res = exec.run("rootfs", &RealVfs::new(), &NullConsole, &[f.display().to_string()]);
        assert!(res.is_err(), "expected cycle error");
    }

    /// Path resolution: stdin sentinel `-` is recognised. We can't actually
    /// pipe to stdin in a unit test cleanly, but we can verify the dispatcher
    /// at least attempts the stdin branch by ensuring `resolve_source("-")`
    /// returns an error only after trying to read (i.e. not "not a file").
    /// In CI stdin is empty → parse_bytes succeeds → returns empty config.
    #[test]
    fn stdin_dash_is_resolved_path() {
        let exec = DefaultExecutor::empty();
        // We just want to confirm "-" is special-cased, not what `resolve_source`
        // returns — it depends on the test runner's stdin. Call via the
        // executor's run() and accept either Ok or an error that is NOT
        // "could not resolve source".
        let res = exec.run("rootfs", &RealVfs::new(), &NullConsole, &["-".to_string()]);
        if let Err(Error::Other(msg)) = &res {
            assert!(
                !msg.contains("could not resolve source"),
                "stdin sentinel was misrouted: {msg}"
            );
        }
    }

    /// Inline YAML detection: contains `:` and `\n` → parsed as YAML.
    #[test]
    fn inline_yaml_is_detected_and_parsed() {
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let exec =
            DefaultExecutor::empty().with_plugin("log", recording_plugin(log.clone(), "p"));
        let inline = "stages:\n  rootfs:\n    - name: inline_one\n".to_string();
        exec.run("rootfs", &RealVfs::new(), &NullConsole, &[inline])
            .unwrap();
        let l = log.lock().unwrap();
        assert!(l.iter().any(|s| s.contains("inline_one")), "got {l:?}");
    }

    /// Inline YAML heuristic: single-line content without `:` + `\n` is NOT
    /// inline-yaml, and the resolver should reject it.
    #[test]
    fn non_yaml_single_token_is_rejected() {
        let exec = DefaultExecutor::empty();
        let res = exec.run("rootfs", &RealVfs::new(), &NullConsole, &["notyaml".to_string()]);
        assert!(res.is_err(), "single-token non-existent path must error");
    }

    /// Modifier applied before parse: dot-notation tokens expand to stage YAML.
    #[test]
    fn modifier_dot_notation_applied_pre_parse() {
        let counter = Arc::new(AtomicUsize::new(0));
        let exec = DefaultExecutor::empty()
            .with_modifier(Arc::new(|b: &[u8]| {
                crate::schema::dot_notation_modifier(b)
            }))
            .with_plugin("count", counter_plugin(counter.clone()));

        // Inline source that LOOKS like dot-notation. We need it to pass the
        // inline-yaml heuristic (contains `:` and `\n`) — so we use a 2-line
        // version with embedded newlines as YAML strings.
        // Easier: write to a file with `.yaml` ext.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("c.yaml");
        fs::write(&f, "stages.rootfs[0].name=fromDot").unwrap();

        exec.run("rootfs", &RealVfs::new(), &NullConsole, &[f.display().to_string()])
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    /// Empty stage name → still runs (no-op).
    #[test]
    fn empty_stage_name_still_runs() {
        let yaml = r#"
stages:
  rootfs:
    - commands: []
"#;
        let counter = Arc::new(AtomicUsize::new(0));
        let exec = DefaultExecutor::empty()
            .with_plugin("count", counter_plugin(counter.clone()));
        let c = cfg(yaml);
        exec.apply("rootfs", &c, &RealVfs::new(), &NullConsole).unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    /// Stage with only conditionals + no plugins → still completes (no-op).
    #[test]
    fn stage_with_no_plugins_is_ok() {
        let exec = DefaultExecutor::empty()
            .with_conditional("always", const_conditional(ConditionalOutcome::Run));
        let c = cfg(ONE_STAGE_YAML);
        let res = exec.apply("rootfs", &c, &RealVfs::new(), &NullConsole);
        assert!(res.is_ok());
    }

    /// Substage non-recursion: running `rootfs.before` should NOT expand to
    /// `rootfs.before.before` etc.
    #[test]
    fn substages_do_not_expand_recursively() {
        let yaml = r#"
stages:
  rootfs.before:
    - name: actual_before
  rootfs.before.before:
    - name: nested
"#;
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let exec =
            DefaultExecutor::empty().with_plugin("log", recording_plugin(log.clone(), "p"));
        let c = cfg(yaml);
        exec.apply("rootfs.before", &c, &RealVfs::new(), &NullConsole).unwrap();
        let l = log.lock().unwrap();
        // Only `actual_before` should run; the `rootfs.before.before` stage
        // must NOT have been picked up by a recursive substage expansion.
        assert!(l.iter().any(|s| s.contains("actual_before")));
        assert!(!l.iter().any(|s| s.contains("nested")), "got {l:?}");
    }

    /// Substages: running `rootfs.after` shouldn't run `rootfs.before` or `rootfs`.
    #[test]
    fn running_after_substage_does_not_run_main() {
        let yaml = r#"
stages:
  rootfs.before:
    - name: pre
  rootfs:
    - name: main
  rootfs.after:
    - name: post
"#;
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let exec =
            DefaultExecutor::empty().with_plugin("log", recording_plugin(log.clone(), "p"));
        let c = cfg(yaml);
        exec.apply("rootfs.after", &c, &RealVfs::new(), &NullConsole).unwrap();
        let l = log.lock().unwrap();
        assert_eq!(*l, vec!["p:post".to_string()]);
    }

    /// substages_for: `rootfs.before` returns single-element list.
    #[test]
    fn substages_for_before_no_expansion() {
        assert_eq!(substages_for("rootfs.before"), vec!["rootfs.before".to_string()]);
    }

    /// substages_for: `rootfs.after` returns single-element list.
    #[test]
    fn substages_for_after_no_expansion() {
        assert_eq!(substages_for("rootfs.after"), vec!["rootfs.after".to_string()]);
    }

    /// substages_for: empty stage name returns one empty element.
    #[test]
    fn substages_for_empty_is_single_empty() {
        assert_eq!(substages_for(""), vec![String::new()]);
    }

    /// substages_for: ordinary name expands to before/main/after.
    #[test]
    fn substages_for_normal_expands_to_three() {
        assert_eq!(
            substages_for("boot"),
            vec!["boot.before".to_string(), "boot".to_string(), "boot.after".to_string()],
        );
    }

    /// `apply` iterates in declaration order (not topological).
    #[test]
    fn apply_uses_declaration_order_not_topo() {
        let yaml = r#"
stages:
  rootfs:
    - name: second
    - name: first
"#;
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let exec =
            DefaultExecutor::empty().with_plugin("log", recording_plugin(log.clone(), "p"));
        let c = cfg(yaml);
        exec.apply("rootfs", &c, &RealVfs::new(), &NullConsole).unwrap();
        let l = log.lock().unwrap();
        assert_eq!(*l, vec!["p:second".to_string(), "p:first".to_string()]);
    }

    /// Substages: missing substage is a silent no-op.
    #[test]
    fn missing_substage_silently_skipped() {
        let yaml = r#"
stages:
  rootfs:
    - name: only
"#;
        let counter = Arc::new(AtomicUsize::new(0));
        let exec = DefaultExecutor::empty()
            .with_plugin("count", counter_plugin(counter.clone()));
        let c = cfg(yaml);
        // No `rootfs.before` and no `rootfs.after` → just `rootfs` runs.
        exec.apply("rootfs", &c, &RealVfs::new(), &NullConsole).unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    /// `run` with multiple sources (paths) runs each in order.
    #[test]
    fn run_with_multiple_sources_runs_each() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.yaml");
        let b = dir.path().join("b.yaml");
        fs::write(&a, "stages:\n  rootfs:\n    - name: from_a\n").unwrap();
        fs::write(&b, "stages:\n  rootfs:\n    - name: from_b\n").unwrap();

        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let exec =
            DefaultExecutor::empty().with_plugin("log", recording_plugin(log.clone(), "p"));
        exec.run(
            "rootfs",
            &RealVfs::new(),
            &NullConsole,
            &[a.display().to_string(), b.display().to_string()],
        )
        .unwrap();
        let l = log.lock().unwrap();
        let pa = l.iter().position(|s| s.contains("from_a")).unwrap();
        let pb = l.iter().position(|s| s.contains("from_b")).unwrap();
        assert!(pa < pb);
    }

    /// `analyze` returns substage ops in order before→main→after.
    #[test]
    fn analyze_returns_substages_in_order() {
        let c = cfg(SUBSTAGE_YAML);
        let exec = DefaultExecutor::empty();
        let names = exec.analyze("rootfs", &c);
        let pa = names.iter().position(|n| n.ends_with(".a")).expect(".a");
        let pb = names.iter().position(|n| n.ends_with(".b")).expect(".b");
        let pc = names.iter().position(|n| n.ends_with(".c")).expect(".c");
        assert!(pa < pb);
        assert!(pb < pc);
    }

    /// Directory walking ignores `.txt`, `.md`, etc.
    #[test]
    fn directory_walk_ignores_non_yaml() {
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let exec =
            DefaultExecutor::empty().with_plugin("log", recording_plugin(log.clone(), "p"));
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.yaml"), "stages:\n  rootfs:\n    - name: ok\n").unwrap();
        fs::write(dir.path().join("b.txt"), "not yaml").unwrap();
        fs::write(dir.path().join("c.json"), "{}").unwrap();
        fs::write(dir.path().join("d.ini"), "[x]").unwrap();
        exec.run(
            "rootfs",
            &RealVfs::new(),
            &NullConsole,
            &[dir.path().display().to_string()],
        )
        .unwrap();
        let l = log.lock().unwrap();
        assert_eq!(l.len(), 1);
        assert!(l[0].contains("ok"));
    }

    /// Files in a directory are walked lexicographically.
    #[test]
    fn directory_walk_is_lexicographic() {
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let exec =
            DefaultExecutor::empty().with_plugin("log", recording_plugin(log.clone(), "p"));
        let dir = tempfile::tempdir().unwrap();
        for (name, label) in &[
            ("03_third.yaml", "third"),
            ("01_first.yaml", "first"),
            ("02_second.yaml", "second"),
        ] {
            fs::write(
                dir.path().join(name),
                format!("stages:\n  rootfs:\n    - name: {label}\n"),
            )
            .unwrap();
        }
        exec.run(
            "rootfs",
            &RealVfs::new(),
            &NullConsole,
            &[dir.path().display().to_string()],
        )
        .unwrap();
        let l = log.lock().unwrap();
        let pf = l.iter().position(|s| s.contains("first")).unwrap();
        let ps = l.iter().position(|s| s.contains("second")).unwrap();
        let pt = l.iter().position(|s| s.contains("third")).unwrap();
        assert!(pf < ps);
        assert!(ps < pt);
    }

    /// looks_like_inline_yaml helper.
    #[test]
    fn inline_yaml_heuristic_basic() {
        assert!(looks_like_inline_yaml("key: value\nother: x"));
        assert!(!looks_like_inline_yaml("just-a-path"));
        assert!(!looks_like_inline_yaml("a: b"), "no newline → not inline yaml");
        assert!(!looks_like_inline_yaml("no_colon\nbut_newline"));
    }

    /// is_url helper.
    #[test]
    fn url_detection() {
        assert!(is_url("http://x.example/y.yaml"));
        assert!(is_url("https://x.example/y.yaml"));
        assert!(!is_url("file:///x"));
        assert!(!is_url("ftp://x"));
        assert!(!is_url("/some/path"));
    }

    /// finish() helper unit tests.
    #[test]
    fn finish_empty_is_ok() {
        let r: Result<()> = finish(Vec::new());
        assert!(r.is_ok());
    }

    #[test]
    fn finish_single_error_passthrough() {
        let r = finish(vec![Error::other("only")]);
        match r {
            Err(Error::Other(msg)) => assert_eq!(msg, "only"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn finish_multiple_errors_wraps_in_multi() {
        let r = finish(vec![Error::other("a"), Error::other("b")]);
        match r {
            Err(Error::Multi(v)) => assert_eq!(v.len(), 2),
            other => panic!("got {other:?}"),
        }
    }

    /// Plugin errors are wrapped in `Error::Plugin { plugin, source }`.
    #[test]
    fn plugin_error_wrap_carries_plugin_name() {
        let exec = DefaultExecutor::empty().with_plugin("widget", failing_plugin("x"));
        let c = cfg(ONE_STAGE_YAML);
        let err = exec
            .apply("rootfs", &c, &RealVfs::new(), &NullConsole)
            .unwrap_err();
        match err {
            Error::Plugin { plugin, .. } => assert_eq!(plugin, "widget"),
            other => panic!("got {other:?}"),
        }
    }

    /// Multiple plugins all run for one stage (Go: every plugin in the chain
    /// is invoked even if previous ones succeeded — no short-circuit).
    #[test]
    fn all_plugins_invoked_per_stage() {
        let a = Arc::new(AtomicUsize::new(0));
        let b = Arc::new(AtomicUsize::new(0));
        let c_ctr = Arc::new(AtomicUsize::new(0));
        let exec = DefaultExecutor::empty()
            .with_plugin("a", counter_plugin(a.clone()))
            .with_plugin("b", counter_plugin(b.clone()))
            .with_plugin("c", counter_plugin(c_ctr.clone()));
        let cfg_ = cfg(ONE_STAGE_YAML);
        exec.apply("rootfs", &cfg_, &RealVfs::new(), &NullConsole).unwrap();
        assert_eq!(a.load(Ordering::SeqCst), 1);
        assert_eq!(b.load(Ordering::SeqCst), 1);
        assert_eq!(c_ctr.load(Ordering::SeqCst), 1);
    }

    /// Apply with a stage_key that doesn't exist in the config → no error.
    #[test]
    fn apply_missing_stage_is_ok() {
        let exec = DefaultExecutor::empty();
        let c = cfg(ONE_STAGE_YAML);
        let res = exec.apply("not_a_real_stage", &c, &RealVfs::new(), &NullConsole);
        assert!(res.is_ok());
    }

    /// `DiskVfs` alias still resolves to `RealVfs`.
    #[test]
    fn disk_vfs_alias_resolves() {
        let _v: DiskVfs = RealVfs::new();
    }

    /// Conditional + plugin order: plugin doesn't run when conditional says Skip.
    #[test]
    fn conditional_skip_short_circuits_chain() {
        let p1 = Arc::new(AtomicUsize::new(0));
        let p2 = Arc::new(AtomicUsize::new(0));
        let exec = DefaultExecutor::empty()
            .with_conditional("nope", const_conditional(ConditionalOutcome::Skip))
            .with_plugin("p1", counter_plugin(p1.clone()))
            .with_plugin("p2", counter_plugin(p2.clone()));
        let c = cfg(ONE_STAGE_YAML);
        exec.apply("rootfs", &c, &RealVfs::new(), &NullConsole).unwrap();
        assert_eq!(p1.load(Ordering::SeqCst), 0);
        assert_eq!(p2.load(Ordering::SeqCst), 0);
    }

    /// Stage with `name` containing dots — should still work as a stage name.
    #[test]
    fn stage_name_with_dots_preserved() {
        let yaml = r#"
stages:
  rootfs:
    - name: "my.dotted.stage"
"#;
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let exec =
            DefaultExecutor::empty().with_plugin("log", recording_plugin(log.clone(), "p"));
        let c = cfg(yaml);
        exec.apply("rootfs", &c, &RealVfs::new(), &NullConsole).unwrap();
        let l = log.lock().unwrap();
        assert_eq!(*l, vec!["p:my.dotted.stage".to_string()]);
    }

    // -----------------------------------------------------------------------
    // Templating preprocess tests (sprig / `{{ .Values.System.* }}`).
    // -----------------------------------------------------------------------

    /// Captures stage.commands so a test can assert on the rendered text.
    fn capturing_plugin(log: Arc<Mutex<Vec<String>>>) -> Plugin {
        Arc::new(move |stage: &Stage, _fs, _con| {
            for cmd in &stage.commands {
                log.lock().unwrap().push(cmd.clone());
            }
            Ok(())
        })
    }

    /// A `{{ .Values.System.OS.Name }}` reference in a command string is
    /// substituted before the modifier runs and before YAML parsing. The
    /// rendered value comes from `gather_sysdata()` and is non-empty on any
    /// reasonable Linux test host, but at minimum the placeholder itself
    /// must NOT survive into the parsed Stage.
    #[test]
    fn template_renders_values_system_os_name() {
        let yaml = r#"
stages:
  rootfs:
    - name: t
      commands:
        - "echo {{ .Values.System.OS.Name }}"
"#;
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let exec =
            DefaultExecutor::empty().with_plugin("cap", capturing_plugin(log.clone()));

        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("c.yaml");
        fs::write(&f, yaml).unwrap();
        exec.run("rootfs", &RealVfs::new(), &NullConsole, &[f.display().to_string()])
            .unwrap();

        let l = log.lock().unwrap();
        assert_eq!(l.len(), 1, "expected exactly one captured command: {l:?}");
        let cmd = &l[0];
        assert!(
            !cmd.contains("{{"),
            "template placeholder leaked through: {cmd:?}"
        );
        assert!(
            cmd.starts_with("echo "),
            "command prefix lost during render: {cmd:?}"
        );
    }

    /// Bad template syntax must not abort parsing. The original (un-rendered)
    /// bytes are passed through to YAML, so the stage parses fine and the
    /// `{{` literal survives into the command.
    #[test]
    fn template_bad_syntax_falls_back_to_raw() {
        // `{{ if }}` with no matching `{{ end }}` is a tera/Go parse error.
        let yaml = r#"
stages:
  rootfs:
    - name: t
      commands:
        - "literal {{ if .x }}unterminated"
"#;
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let exec =
            DefaultExecutor::empty().with_plugin("cap", capturing_plugin(log.clone()));

        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("c.yaml");
        fs::write(&f, yaml).unwrap();
        // Run must succeed — bad templates are swallowed (Go parity).
        exec.run("rootfs", &RealVfs::new(), &NullConsole, &[f.display().to_string()])
            .unwrap();

        let l = log.lock().unwrap();
        assert_eq!(l.len(), 1, "expected fallback-parsed command: {l:?}");
        // Raw bytes were fed to YAML, so the `{{` literal is preserved.
        assert!(
            l[0].contains("{{ if .x }}"),
            "raw template text should survive fallback: {:?}",
            l[0]
        );
    }

    /// A config without any `{{ ... }}` segments passes through templating
    /// unchanged — render is effectively a no-op for plain YAML.
    #[test]
    fn template_passthrough_for_plain_yaml() {
        let yaml = r#"
stages:
  rootfs:
    - name: plain
      commands:
        - "echo no templating here"
"#;
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let exec =
            DefaultExecutor::empty().with_plugin("cap", capturing_plugin(log.clone()));

        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("c.yaml");
        fs::write(&f, yaml).unwrap();
        exec.run("rootfs", &RealVfs::new(), &NullConsole, &[f.display().to_string()])
            .unwrap();

        let l = log.lock().unwrap();
        assert_eq!(*l, vec!["echo no templating here".to_string()]);
    }

    /// Build a stage with both `commands` and `files` populated for the
    /// command/file count logging path (we just verify it doesn't panic).
    #[test]
    fn stage_with_both_files_and_commands_runs() {
        let yaml = r#"
stages:
  rootfs:
    - name: mixed
      commands: ["echo hi"]
      files:
        - path: /tmp/x
          content: y
"#;
        let counter = Arc::new(AtomicUsize::new(0));
        let exec = DefaultExecutor::empty()
            .with_plugin("count", counter_plugin(counter.clone()));
        let c = cfg(yaml);
        exec.apply("rootfs", &c, &RealVfs::new(), &NullConsole).unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}
