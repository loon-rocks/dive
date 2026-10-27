use crate::user_tools::{ToolKind, UserToolDoc, USER_TOOL_PREFIX};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TestRunPreview {
    pub tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    pub runs: RunKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub input: Value,
    pub effects: Effects,
    pub flagged: Vec<usize>,
    pub secrets: Vec<String>,
    pub unset: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub uses: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    Automatic,
    Remembered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    Exec,
    Prompt,
    Reshape,
    Decision,
    Image,
    Agent,
    Plan,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Effects {
    pub writes: Option<bool>,
    pub network: Option<bool>,
}

impl TestRunPreview {
    pub fn new(doc: &UserToolDoc, purpose: Option<&str>, input: &Value, unset: &[String]) -> Self {
        let runs = match &doc.kind {
            ToolKind::Exec { .. } => RunKind::Exec,
            ToolKind::Prompt { .. } => RunKind::Prompt,
            ToolKind::Reshape { .. } => RunKind::Reshape,
            ToolKind::Decision { .. } => RunKind::Decision,
            ToolKind::Image { .. } => RunKind::Image,
        };
        let (program, command, scan) = match &doc.kind {
            ToolKind::Exec { command, args, .. } => {
                let line = command_line(command, args);
                let scan = scan(command, args, &line);
                (Some(program_name(command)), Some(line), scan)
            }
            _ => (None, None, Scan::default()),
        };
        Self {
            tool: format!("{USER_TOOL_PREFIX}{}", doc.name),
            purpose: purpose
                .map(str::trim)
                .filter(|purpose| !purpose.is_empty())
                .map(str::to_string),
            runs,
            program,
            command,
            input: input.clone(),
            effects: scan.effects,
            flagged: scan.flagged,
            secrets: crate::user_tools::needed_secrets(doc)
                .into_iter()
                .map(|secret| secret.name)
                .collect(),
            unset: unset.to_vec(),
            read_only: doc.read_only,
            uses: Vec::new(),
        }
    }

    pub fn network(
        entry: &str,
        runs: RunKind,
        purpose: Option<&str>,
        input: &Value,
        tools: &[UserToolDoc],
        secrets: Vec<String>,
    ) -> Self {
        let scans: Vec<Effects> = tools
            .iter()
            .map(|doc| Self::new(doc, None, &Value::Null, &[]).effects)
            .collect();
        let found = |pick: fn(&Effects) -> Option<bool>| {
            scans
                .iter()
                .any(|effects| pick(effects) == Some(true))
                .then_some(true)
        };
        Self {
            tool: entry.to_string(),
            purpose: purpose
                .map(str::trim)
                .filter(|purpose| !purpose.is_empty())
                .map(str::to_string),
            runs,
            program: None,
            command: None,
            input: input.clone(),
            effects: Effects {
                writes: found(|effects| effects.writes),
                network: found(|effects| effects.network),
            },
            flagged: Vec::new(),
            secrets,
            unset: Vec::new(),
            read_only: None,
            uses: tools
                .iter()
                .map(|doc| format!("{USER_TOOL_PREFIX}{}", doc.name))
                .collect(),
        }
    }

    pub fn safe_to_run_unasked(&self) -> bool {
        match self.runs {
            RunKind::Prompt | RunKind::Reshape | RunKind::Decision => true,
            RunKind::Image | RunKind::Agent | RunKind::Plan => false,
            RunKind::Exec => {
                self.read_only == Some(true)
                    && self.effects.writes == Some(false)
                    && self.effects.network == Some(false)
                    && self.secrets.is_empty()
            }
        }
    }

    pub fn prompt(&self) -> String {
        let what = match self.runs {
            RunKind::Exec => format!("runs: {}", self.command.as_deref().unwrap_or_default()),
            RunKind::Prompt => "makes one model call with its prompt".to_string(),
            RunKind::Reshape => "reshapes its input, with no side effects".to_string(),
            RunKind::Decision => "asks the decision model its questions".to_string(),
            RunKind::Image => "generates an image with the image model".to_string(),
            RunKind::Agent | RunKind::Plan => {
                "runs for real, tools and each agent's own model included, and saves nothing"
                    .to_string()
            }
        };
        let mut prompt = match self.runs {
            RunKind::Agent | RunKind::Plan => format!("Test the network from {}?", self.tool),
            _ => format!("Test-run the drafted tool {} once?", self.tool),
        };
        if let Some(purpose) = &self.purpose {
            prompt.push(' ');
            prompt.push_str(purpose);
        }
        prompt.push_str(&format!(
            "\n\nIt {what}\n\nwith input: {}",
            serde_json::to_string_pretty(&self.input).unwrap_or_default()
        ));
        let found: Vec<&str> = [
            (self.effects.writes == Some(true)).then_some("writes files"),
            (self.effects.network == Some(true)).then_some("uses the network"),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !found.is_empty() {
            prompt.push_str(&format!("\n\nIt looks like it {}.", found.join(" and ")));
        }
        if !self.unset.is_empty() {
            prompt.push_str(&format!(
                "\n\nIt needs {}, which isn't set. Set it (dive secret set NAME), then approve.",
                self.unset.join(", ")
            ));
        }
        prompt
    }
}

fn program_name(command: &str) -> String {
    command.rsplit('/').next().unwrap_or(command).to_string()
}

fn command_line(command: &str, args: &[String]) -> String {
    let mut line = command.to_string();
    for arg in args {
        line.push(' ');
        if arg.contains('\n') {
            line.push_str(&format!("'\n{}\n'", arg.trim_matches('\n')));
        } else if arg.is_empty()
            || arg.contains(|c: char| c.is_whitespace() || "'\"$`\\;&|<>(){}*?".contains(c))
        {
            line.push('\'');
            line.push_str(&arg.replace('\'', "'\\''"));
            line.push('\'');
        } else {
            line.push_str(arg);
        }
    }
    line
}

#[derive(Default)]
struct Scan {
    effects: Effects,
    flagged: Vec<usize>,
}

const SHELLS: [&str; 5] = ["sh", "bash", "zsh", "dash", "fish"];

const INLINE_INTERPRETERS: [&str; 8] = [
    "python",
    "python3",
    "node",
    "ruby",
    "perl",
    "deno",
    "bun",
    "osascript",
];

const READ_ONLY_PROGRAMS: [&str; 18] = [
    "printf", "echo", "cat", "jq", "date", "ls", "grep", "rg", "head", "tail", "wc", "sort",
    "uniq", "cut", "tr", "pwd", "env", "basename",
];

const WRITE_WORDS: [&str; 12] = [
    "rm", "mv", "cp", "mkdir", "rmdir", "touch", "tee", "dd", "chmod", "chown", "ln", "truncate",
];

const NETWORK_WORDS: [&str; 9] = [
    "curl", "wget", "ssh", "scp", "rsync", "nc", "ncat", "telnet", "gh",
];

const OPEN_MODES: [&str; 16] = [
    "\"w\"", "'w'", "\"wb\"", "'wb'", "\"w+\"", "'w+'", "\"a\"", "'a'", "\"ab\"", "'ab'", "\"a+\"",
    "'a+'", "\"x\"", "'x'", "\"r+\"", "'r+'",
];

const WRITE_PATTERNS: [&str; 18] = [
    "write_text(",
    "write_bytes(",
    "os.remove(",
    "os.unlink(",
    "os.rename(",
    "os.replace(",
    "os.makedirs(",
    "os.mkdir(",
    "os.rmdir(",
    "shutil.",
    ".unlink(",
    ".mkdir(",
    ".touch(",
    "writeFile",
    "appendFile",
    "mkdirSync",
    "unlinkSync",
    "sed -i",
];

const NETWORK_PATTERNS: [&str; 13] = [
    "http://",
    "https://",
    "requests",
    "urllib",
    "httpx",
    "aiohttp",
    "socket",
    "http.client",
    "fetch(",
    "net/http",
    "git clone",
    "git push",
    "git pull",
];

const UNSEEN_PATTERNS: [&str; 10] = [
    "subprocess",
    "os.system",
    "os.popen",
    "child_process",
    "exec(",
    "eval(",
    "spawn",
    "Popen",
    "`",
    "$(",
];

const PYTHON_STDLIB: [&str; 46] = [
    "sys",
    "os",
    "json",
    "re",
    "math",
    "datetime",
    "time",
    "pathlib",
    "collections",
    "itertools",
    "functools",
    "typing",
    "string",
    "textwrap",
    "csv",
    "hashlib",
    "base64",
    "uuid",
    "random",
    "statistics",
    "decimal",
    "fractions",
    "io",
    "glob",
    "fnmatch",
    "shlex",
    "argparse",
    "dataclasses",
    "enum",
    "operator",
    "copy",
    "unicodedata",
    "html",
    "xml",
    "zlib",
    "gzip",
    "struct",
    "calendar",
    "difflib",
    "pprint",
    "traceback",
    "bisect",
    "heapq",
    "locale",
    "codecs",
    "binascii",
];

const NODE_BUILTINS: [&str; 6] = ["fs", "path", "os", "util", "crypto", "process"];

fn words(line: &str) -> impl Iterator<Item = &str> {
    line.split(|c: char| c.is_whitespace() || ";|&()[]{},\"'".contains(c))
        .filter(|word| !word.is_empty())
}

fn shell_redirects(line: &str) -> bool {
    let bytes: Vec<char> = line.chars().collect();
    for (at, c) in bytes.iter().enumerate() {
        if *c != '>' {
            continue;
        }
        let rest: String = bytes[at + 1..].iter().collect();
        let target = rest.trim_start_matches('>').trim_start();
        if target.starts_with('&')
            || target.starts_with("/dev/null")
            || target.starts_with("/dev/std")
        {
            continue;
        }
        if !target.is_empty() {
            return true;
        }
    }
    false
}

fn module_root(name: &str) -> &str {
    name.split('.').next().unwrap_or(name).trim()
}

fn outside_imports(program: &str, line: &str) -> bool {
    let trimmed = line.trim_start();
    if program.starts_with("python") {
        let names: Vec<&str> = if let Some(rest) = trimmed.strip_prefix("import ") {
            rest.split(',')
                .map(|name| module_root(name.split(" as ").next().unwrap_or(name)))
                .collect()
        } else if let Some(rest) = trimmed.strip_prefix("from ") {
            vec![module_root(
                rest.split_whitespace().next().unwrap_or_default(),
            )]
        } else {
            Vec::new()
        };
        return names
            .iter()
            .any(|name| !name.is_empty() && !PYTHON_STDLIB.contains(name));
    }
    if program == "node" || program == "deno" || program == "bun" {
        let mut rest = trimmed;
        while let Some(at) = rest.find("require(").or_else(|| rest.find("from ")) {
            rest = &rest[at..];
            let quoted = rest
                .split(['"', '\''])
                .nth(1)
                .unwrap_or_default()
                .trim_start_matches("node:");
            if !NODE_BUILTINS.contains(&quoted) {
                return true;
            }
            rest = &rest[1..];
        }
    }
    false
}

fn scan(command: &str, args: &[String], line: &str) -> Scan {
    let program = program_name(command);
    let shell = SHELLS.contains(&program.as_str());
    let inline = (shell || INLINE_INTERPRETERS.contains(&program.as_str()))
        && args.iter().any(|arg| arg == "-c" || arg == "-e");
    let whole = inline || READ_ONLY_PROGRAMS.contains(&program.as_str());
    let mut writes = WRITE_WORDS.contains(&program.as_str());
    let mut network = NETWORK_WORDS.contains(&program.as_str());
    let mut unseen = !whole;
    let mut foreign = false;
    let mut flagged = Vec::new();
    for (at, text) in line.lines().enumerate() {
        let word = |list: &[&str]| words(text).any(|word| list.contains(&word));
        let writes_here = WRITE_PATTERNS.iter().any(|pattern| text.contains(pattern))
            || (text.contains("open(") && OPEN_MODES.iter().any(|mode| text.contains(mode)))
            || (shell && (word(&WRITE_WORDS) || shell_redirects(text)));
        let network_here = NETWORK_PATTERNS
            .iter()
            .any(|pattern| text.contains(pattern))
            || word(&NETWORK_WORDS);
        if writes_here || network_here {
            flagged.push(at);
        }
        writes |= writes_here;
        network |= network_here;
        unseen |= UNSEEN_PATTERNS.iter().any(|pattern| text.contains(pattern));
        foreign |= outside_imports(&program, text);
    }
    let ruled_out = |found: bool, extra: bool| {
        if found {
            Some(true)
        } else if unseen || extra {
            None
        } else {
            Some(false)
        }
    };
    Scan {
        effects: Effects {
            writes: ruled_out(writes, foreign),
            network: ruled_out(network, foreign),
        },
        flagged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn exec(command: &str, args: &[&str], read_only: Option<bool>) -> TestRunPreview {
        let yaml = format!(
            "name: t\ndescription: t\nkind: exec\ncommand: {command}\nargs: {}\n{}",
            serde_json::to_string(args).unwrap(),
            read_only
                .map(|flag| format!("read_only: {flag}\n"))
                .unwrap_or_default()
        );
        let doc = crate::user_tools::parse_tool_source(&yaml).unwrap();
        TestRunPreview::new(&doc, Some("Check it"), &json!({}), &[])
    }

    const READ_FILE: &str = "import sys, os, json\nraw = sys.argv[1] if len(sys.argv) > 1 else \"\"\nif not raw: sys.stderr.write(\"empty\\n\"); sys.exit(1)\nwith open(raw, \"rb\") as f:\n    data = f.read()\nprint(json.dumps({\"content\": data.decode()}))";

    #[test]
    fn an_inline_script_that_only_reads_is_ruled_out_for_writes_and_network() {
        let preview = exec("python3", &["-c", READ_FILE, "{{input.path}}"], Some(true));
        assert_eq!(
            preview.effects,
            Effects {
                writes: Some(false),
                network: Some(false)
            }
        );
        assert!(preview.flagged.is_empty());
        assert!(preview.safe_to_run_unasked());
        assert_eq!(preview.program.as_deref(), Some("python3"));
        assert!(preview
            .command
            .unwrap()
            .contains("\nimport sys, os, json\n"));
    }

    #[test]
    fn a_read_only_tool_still_asks_unless_it_says_so() {
        let preview = exec("python3", &["-c", READ_FILE], None);
        assert_eq!(preview.effects.writes, Some(false));
        assert!(!preview.safe_to_run_unasked());
    }

    #[test]
    fn writing_a_file_is_found_and_its_lines_are_flagged() {
        let script = "import sys\nwith open(sys.argv[1], \"w\") as f:\n    f.write(sys.argv[2])\nprint(\"{}\")";
        let preview = exec("python3", &["-c", script, "a", "b"], Some(true));
        assert_eq!(preview.effects.writes, Some(true));
        assert_eq!(preview.effects.network, Some(false));
        assert_eq!(preview.flagged, vec![2]);
        assert!(!preview.safe_to_run_unasked());
    }

    #[test]
    fn curl_is_network_and_an_unknown_import_leaves_network_unsure() {
        let curl = exec("curl", &["-s", "https://example.com"], Some(true));
        assert_eq!(curl.effects.network, Some(true));
        assert_eq!(curl.effects.writes, None);
        let script = "import sys, json\nimport boto3\nprint(json.dumps({}))";
        let foreign = exec("python3", &["-c", script], Some(true));
        assert_eq!(foreign.effects.network, None);
        assert!(!foreign.safe_to_run_unasked());
    }

    #[test]
    fn a_script_file_or_subprocess_is_unsure_not_ruled_out() {
        let file = exec("python3", &["tool.py"], Some(true));
        assert_eq!(file.effects, Effects::default());
        let sub = exec(
            "python3",
            &["-c", "import subprocess\nsubprocess.run([\"ls\"])"],
            Some(true),
        );
        assert_eq!(sub.effects, Effects::default());
    }

    #[test]
    fn a_shell_redirect_writes_but_stderr_and_dev_null_do_not() {
        let quiet = exec(
            "sh",
            &["-c", "ls 2>/dev/null >&2; echo hi 1>&2"],
            Some(true),
        );
        assert_eq!(quiet.effects.writes, Some(false));
        let loud = exec("sh", &["-c", "echo hi > out.txt"], Some(true));
        assert_eq!(loud.effects.writes, Some(true));
    }

    #[test]
    fn a_network_test_carries_the_effects_its_drafted_tools_were_found_to_have() {
        let tool = |name: &str, script: &str| {
            crate::user_tools::parse_tool_source(&format!(
                "name: {name}\ndescription: t\nkind: exec\ncommand: sh\nargs: {}\n",
                serde_json::to_string(&["-c", script]).unwrap()
            ))
            .unwrap()
        };
        let preview = TestRunPreview::network(
            "agent__notes",
            RunKind::Agent,
            Some("Check it files a note."),
            &json!({"message": "save hello"}),
            &[tool("save", "echo hi > note.txt"), tool("look", "ls")],
            vec!["TOKEN".to_string()],
        );
        assert_eq!(
            preview.effects,
            Effects {
                writes: Some(true),
                network: None
            }
        );
        assert_eq!(preview.uses, vec!["user__save", "user__look"]);
        assert!(!preview.safe_to_run_unasked());
        assert!(preview
            .prompt()
            .starts_with("Test the network from agent__notes? Check it files a note."));
    }

    #[test]
    fn printf_is_read_only_and_the_prompt_names_the_purpose() {
        let preview = exec("printf", &["{\"a\":%s}", "{{input.a}}"], None);
        assert_eq!(preview.effects.writes, Some(false));
        let prompt = preview.prompt();
        assert!(
            prompt.starts_with("Test-run the drafted tool user__t once? Check it"),
            "{prompt}"
        );
        assert!(prompt.contains("printf"), "{prompt}");
    }
}
