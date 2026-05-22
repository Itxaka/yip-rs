//! `DefaultExecutor` — port of `pkg/executor/default.go`.
//!
//! Differences from the Go version, with justification:
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
pub struct DefaultExecutor {
    pub plugins: Vec<(String, Plugin)>,
    pub conditionals: Vec<(String, Conditional)>,
    /// Optional pre-parse modifier (e.g. `dot_notation_modifier`). Applied
    /// to every raw config blob before YAML parsing.
    pub modifier: Option<Arc<dyn Fn(&[u8]) -> Result<Vec<u8>> + Send + Sync>>,
}

impl DefaultExecutor {
    /// Construct an executor with the default plugin + conditional set
    /// (matches Go `NewExecutor()`).
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
    pub fn empty() -> Self {
        Self {
            plugins: Vec::new(),
            conditionals: Vec::new(),
            modifier: None,
        }
    }

    /// Register a plugin. Order matters — plugins run in registration order.
    pub fn with_plugin(mut self, name: &str, p: Plugin) -> Self {
        self.plugins.push((name.to_string(), p));
        self
    }

    /// Register a conditional. Order matters — conditionals run in
    /// registration order until one returns `Skip`.
    pub fn with_conditional(mut self, name: &str, c: Conditional) -> Self {
        self.conditionals.push((name.to_string(), c));
        self
    }

    /// Replace the pre-parse modifier.
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
        // The template module may be a stub during early waves; if so it
        // should be a pass-through. Errors propagate.
        // TODO(wave-5): once `crate::template::render_with_sysdata` lands,
        // switch to it for the richer funcmap.
        #[allow(unused_mut)]
        let mut rendered = render_template(bytes)?;

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

/// Template-render a raw config blob. Wave-5 fills in the real impl; for
/// now this is a pass-through so we don't block on it.
fn render_template(bytes: &[u8]) -> Result<Vec<u8>> {
    // Once `crate::template::render_with_sysdata` exists, replace this body
    // with a call to it. The signature should be:
    //   pub fn render_with_sysdata(bytes: &[u8]) -> Result<Vec<u8>>;
    Ok(bytes.to_vec())
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
}
