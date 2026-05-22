//! Console abstraction: shell-out (`StandardConsole`) and recording mock
//! (`RecordingConsole`). Ports `pkg/console/` and the `plugins.Console`
//! interface from `pkg/plugins/common.go`.

mod console;

pub use console::{Console, RecordedCall, RecordingConsole, StandardConsole};
