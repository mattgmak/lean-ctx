//! Shell-specific completion script generators and output formatters.

use super::engine::Completion;

/// Generate a zsh completion script that delegates to `lean-ctx __complete`.
pub(super) fn zsh_script() -> String {
    r#"#compdef lean-ctx lctx _lc _lc_compress

_lean-ctx() {
    local -a completions
    local IFS=$'\n'
    completions=(${(f)"$(lean-ctx __complete zsh -- "${words[@]:1}")" })
    if (( ${#completions} )); then
        _describe -t commands 'lean-ctx' completions
    fi
}

compdef _lean-ctx lean-ctx 2>/dev/null
compdef _lean-ctx lctx 2>/dev/null
compdef _lean-ctx _lc 2>/dev/null
compdef _lean-ctx _lc_compress 2>/dev/null
"#
    .to_string()
}

/// Generate a bash completion script that delegates to `lean-ctx __complete`.
pub(super) fn bash_script() -> String {
    r#"_lean_ctx_complete() {
    local cur="${COMP_WORDS[COMP_CWORD]}"
    local IFS=$'\n'
    COMPREPLY=($(lean-ctx __complete bash -- "${COMP_WORDS[@]:1}"))
}

complete -F _lean_ctx_complete lean-ctx
complete -F _lean_ctx_complete lctx
complete -F _lean_ctx_complete _lc
complete -F _lean_ctx_complete _lc_compress
"#
    .to_string()
}

/// Generate a fish completion script that delegates to `lean-ctx __complete`.
pub(super) fn fish_script() -> String {
    r"function __lean_ctx_complete
    set -l tokens (commandline -opc)
    set -e tokens[1]
    set -l current (commandline -ct)
    set tokens $tokens $current
    lean-ctx __complete fish -- $tokens 2>/dev/null
end

complete -c lean-ctx -f -a '(__lean_ctx_complete)'
complete -c lctx -f -a '(__lean_ctx_complete)'
complete -c _lc -f -a '(__lean_ctx_complete)'
complete -c _lc_compress -f -a '(__lean_ctx_complete)'
"
    .to_string()
}

/// Format completions for zsh: `value:description` per line.
pub(super) fn format_zsh(completions: &[Completion]) -> String {
    let mut out = String::new();
    for c in completions {
        let desc = if c.description.is_empty() {
            String::new()
        } else {
            format!(":{}", c.description.replace(':', "\\:"))
        };
        out.push_str(&c.value);
        out.push_str(&desc);
        out.push('\n');
    }
    out
}

/// Format completions for bash: one value per line.
pub(super) fn format_bash(completions: &[Completion]) -> String {
    let mut out = String::new();
    for c in completions {
        out.push_str(&c.value);
        out.push('\n');
    }
    out
}

/// Format completions for fish: `value\tdescription` per line.
pub(super) fn format_fish(completions: &[Completion]) -> String {
    let mut out = String::new();
    for c in completions {
        out.push_str(&c.value);
        if !c.description.is_empty() {
            out.push('\t');
            out.push_str(&c.description);
        }
        out.push('\n');
    }
    out
}
