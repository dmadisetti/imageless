//! Parsing of `nix`-shebang scripts into the packages and entrypoint a
//! generated seed flake needs.
//!
//! This is a client-side desugaring and nothing more. The spec, the node, and
//! the shim never learn that shebangs exist: what leaves here is an ordinary
//! seed directory whose `flake.nix` builds `#rootfs`, indistinguishable from
//! one a person wrote. The grammar recognized is the `nix shell` form from the
//! Nix manual —
//!
//! ```text
//! #!/usr/bin/env nix
//! #! nix shell nixpkgs#bash nixpkgs#cowsay --command bash
//! ```
//!
//! — and only that form. Every other spelling is refused with the reason,
//! because a shebang that is *almost* understood and quietly reinterpreted is
//! worse than one that is rejected: the script would run, against packages
//! nobody named.

/// A parsed `nix shell` shebang.
#[cfg_attr(test, derive(Debug))]
pub struct Shebang {
    /// Attribute paths into nixpkgs, in the order the shebang named them,
    /// duplicates removed. `nixpkgs#python3` yields `python3`.
    pub packages: Vec<String>,
    /// The `--command` word: the binary the pod invokes out of the generated
    /// environment's `bin/`. Usually one of `packages` by name, but not
    /// required to be — see the warning `parse` attaches when it is not.
    pub interpreter: String,
    /// Arguments the shebang passes to the interpreter ahead of the script.
    pub interpreter_arguments: Vec<String>,
    /// Things that are probably wrong but that this side cannot decide without
    /// evaluating, which it has no Nix to do. The caller prints them.
    pub warnings: Vec<String>,
    /// How many leading lines are shebang. The generator blanks exactly these,
    /// because a `#!` continuation line is not universally a comment.
    pub shebang_lines: usize,
}

/// Recognizes the interpreter line alone. Cheap enough to run against any
/// candidate file, and it decides whether a path is a shebang deployable at
/// all — so it is deliberately narrow: `nix` must be the program, not merely a
/// word on the line.
pub fn is_nix_shebang(contents: &str) -> bool {
    let Some(first) = contents.lines().next() else {
        return false;
    };
    let Some(rest) = first.strip_prefix("#!") else {
        return false;
    };
    let mut words = rest.split_whitespace();
    match words.next() {
        // `#!/usr/bin/env nix` — env takes the program as its first non-flag
        // argument. Flags to env would change what runs, so they are not
        // waved through.
        Some(program) if basename(program) == "env" => words.next() == Some("nix"),
        // `#!/nix/store/…/bin/nix` or `#!/usr/bin/nix`.
        Some(program) => basename(program) == "nix",
        None => false,
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub fn parse(contents: &str) -> Result<Shebang, String> {
    if !is_nix_shebang(contents) {
        // `#!/usr/bin/env -S nix shell …` is the natural one-line spelling and
        // is not what Nix documents, so it lands here — with an error about the
        // first line that says nothing about the `-S`. Name it.
        let first = contents.lines().next().unwrap_or_default();
        if first.starts_with("#!") && first.contains(" -S ") && first.contains("nix") {
            return Err(
                "`env -S` puts the whole invocation on the interpreter line; imageless reads the \
                 two-line form the Nix manual documents — `#!/usr/bin/env nix` followed by \
                 `#! nix shell …`"
                    .to_string(),
            );
        }
        return Err("the first line must be a nix shebang (#!/usr/bin/env nix)".to_string());
    }
    let mut words = Vec::new();
    let mut shebang_lines = 1;
    // Nix concatenates the arguments of every `#!` line after the first, so a
    // long package list can wrap. Reproduce that rather than reading only the
    // second line.
    for (index, line) in contents.lines().enumerate().skip(1) {
        let Some(rest) = line.strip_prefix("#!") else {
            break;
        };
        shebang_lines += 1;
        let mut line_words = rest.split_whitespace();
        match line_words.next() {
            Some("nix") => {}
            Some(other) => {
                return Err(format!(
                    "shebang line {} runs `{other}`, not `nix` — \
                     imageless desugars nix shebangs only",
                    index + 1
                ))
            }
            None => continue,
        }
        words.extend(line_words.map(str::to_string));
    }
    if words.is_empty() {
        return Err("no `#! nix …` line follows the interpreter line".to_string());
    }
    // Quoting is the one place where splitting on whitespace and doing what
    // the author meant diverge, and silently dropping a quote would build a
    // package named `"my` — so say so instead.
    if let Some(quoted) = words
        .iter()
        .find(|word| word.contains('"') || word.contains('\''))
    {
        return Err(format!(
            "{quoted}: shebang arguments are split on whitespace, so quotes are \
             not interpreted — write the argument without them"
        ));
    }

    let mut arguments = words.iter().map(String::as_str);
    match arguments.next() {
        Some("shell") => {}
        Some(other) => {
            return Err(format!(
                "`nix {other}` is not supported — write `nix shell … --command <interpreter>`, \
                 the form the Nix manual documents for shebangs"
            ))
        }
        None => unreachable!("words is non-empty"),
    }

    let mut packages = Vec::new();
    let mut command = Vec::new();
    let mut after_command = false;
    for argument in arguments {
        if after_command {
            command.push(argument.to_string());
            continue;
        }
        match argument {
            "--command" | "-c" => after_command = true,
            flag if flag.starts_with('-') => {
                return Err(format!(
                    "{flag}: imageless generates the flake rather than running `nix shell`, so \
                     flags that change how the shell is built have no meaning here — remove it"
                ))
            }
            installable => {
                let package = attribute_path(installable)?;
                if !packages.contains(&package) {
                    packages.push(package);
                }
            }
        }
    }

    if packages.is_empty() {
        return Err("the shebang names no packages — nothing would be in the rootfs".to_string());
    }
    let mut command = command.into_iter();
    let Some(interpreter) = command.next() else {
        return Err(
            "the shebang has no `--command <interpreter>` — imageless needs to know what runs \
             the script"
                .to_string(),
        );
    };
    // Every other token that reaches generated Nix or a generated path is
    // constrained; this one used to be the exception, and it is interpolated
    // into three of them: the pod's `/bin/{interpreter}`, the generated
    // passwd shell, and a `''` string in the flake. `--command /bin/bash`
    // therefore produced `/bin//bin/bash`, which POSIX collapses to
    // `/bin/bin/bash` — a path nothing in a sparse rootfs provides — and
    // `${` opened an interpolation in the flake. An absolute path cannot be
    // honored here (the rootfs holds only buildEnv's symlinks) and silently
    // rewriting it to its basename would be the quiet reinterpretation this
    // module refuses to do, so say no.
    if interpreter.contains('/') {
        return Err(format!(
            "--command {interpreter}: imageless runs the interpreter out of the generated \
             environment's bin/, so this has to be a bare binary name — write the name alone, \
             and add the package that provides it to the shebang"
        ));
    }
    if let Some(bad) = interpreter
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+')))
    {
        return Err(format!(
            "--command {interpreter}: `{bad}` is not legal in a binary name"
        ));
    }
    // A warning and not a refusal, because the client cannot tell whether a
    // package provides a binary — that takes evaluation, and this side has no
    // Nix. The name match is a heuristic that holds for the usual interpreters
    // (bash, python3, perl) and breaks for the equally usual ones:
    // `nixpkgs#nodejs --command node` and `nixpkgs#coreutils --command ls` are
    // both correct, and there is no `nixpkgs#node` or `nixpkgs#ls` to suggest.
    // Refusing those would block a valid deploy on advice that cannot be
    // followed; accepting a genuine typo costs a pod that fails at start with
    // the interpreter's own "no such file", which is visible and cheap.
    let mut warnings = Vec::new();
    if !packages.iter().any(|package| package == &interpreter) {
        warnings.push(format!(
            "--command {interpreter} matches no package name in the shebang. That is fine when \
             the binary's name differs from its attribute (nodejs provides node), and a typo \
             otherwise — in which case the pod starts and fails to exec /bin/{interpreter}"
        ));
    }

    Ok(Shebang {
        packages,
        interpreter,
        interpreter_arguments: command.collect(),
        warnings,
        shebang_lines,
    })
}

/// `nixpkgs#python3` → `python3`.
///
/// Only the `nixpkgs#` prefix is accepted. A generated flake has exactly one
/// input, and widening that to arbitrary flake references would mean
/// generating inputs the client cannot lock — it has no Nix with which to
/// compute a `narHash`, and an input pinned by nothing is the unreproducible
/// deploy this mode exists to avoid.
fn attribute_path(installable: &str) -> Result<String, String> {
    let Some(attribute) = installable.strip_prefix("nixpkgs#") else {
        return if installable.contains('#') {
            Err(format!(
                "{installable}: only nixpkgs# installables are supported — the generated flake \
                 pins one input, and the client has no Nix with which to lock another"
            ))
        } else {
            Err(format!(
                "{installable}: write it as nixpkgs#{installable} — a bare name would be \
                 resolved against the caller's registry, which is not a pin"
            ))
        };
    };
    if attribute.is_empty() {
        return Err(format!("{installable}: names no attribute"));
    }
    // The attribute is interpolated into generated Nix source, so anything
    // that is not an attribute path has to be refused here rather than
    // producing a flake that fails to parse on the node.
    let legal = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '\'');
    if !attribute.chars().all(legal) {
        return Err(format!(
            "{installable}: `{attribute}` is not an attribute path (letters, digits, and -_.' only)"
        ));
    }
    if attribute.starts_with('.') || attribute.ends_with('.') || attribute.contains("..") {
        return Err(format!(
            "{installable}: `{attribute}` is not an attribute path"
        ));
    }
    Ok(attribute.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nix_shell_shebang_yields_its_packages_and_interpreter() {
        let script = "#!/usr/bin/env nix\n\
                      #! nix shell nixpkgs#bash nixpkgs#cowsay --command bash\n\
                      cowsay hello\n";
        let parsed = parse(script).expect("parses");
        assert_eq!(parsed.packages, ["bash", "cowsay"]);
        assert_eq!(parsed.interpreter, "bash");
        assert!(parsed.interpreter_arguments.is_empty());
    }

    #[test]
    fn arguments_accumulate_across_every_shebang_line() {
        // The Nix manual's continuation form: every line repeats `nix`, the
        // subcommand appears once, and the arguments concatenate.
        let script = "#!/usr/bin/env nix\n\
                      #! nix shell nixpkgs#python3\n\
                      #! nix nixpkgs#curl --command python3 -u\n\
                      print('hi')\n";
        let parsed = parse(script).expect("parses");
        assert_eq!(parsed.packages, ["python3", "curl"]);
        assert_eq!(parsed.interpreter, "python3");
        assert_eq!(parsed.interpreter_arguments, ["-u"]);
    }

    #[test]
    fn a_repeated_package_is_named_once_in_the_generated_environment() {
        let script = "#!/usr/bin/env nix\n\
                      #! nix shell nixpkgs#bash nixpkgs#bash --command bash\n";
        assert_eq!(parse(script).expect("parses").packages, ["bash"]);
    }

    /// A warning, not a refusal: `nixpkgs#nodejs --command node` is correct and
    /// there is no `nixpkgs#node` to suggest, so refusing would block a valid
    /// deploy on advice that cannot be followed.
    #[test]
    fn an_interpreter_matching_no_package_name_warns_rather_than_refusing() {
        let script = "#!/usr/bin/env nix\n\
                      #! nix shell nixpkgs#nodejs --command node\n";
        let parsed = parse(script).expect("parses");
        assert_eq!(parsed.interpreter, "node");
        assert_eq!(parsed.warnings.len(), 1, "{:?}", parsed.warnings);
        assert!(
            parsed.warnings[0].contains("nodejs"),
            "{:?}",
            parsed.warnings
        );

        let matching = "#!/usr/bin/env nix\n#! nix shell nixpkgs#bash --command bash\n";
        assert!(parse(matching).expect("parses").warnings.is_empty());
    }

    #[test]
    fn the_one_line_env_dash_s_form_is_refused_by_name() {
        let script = "#!/usr/bin/env -S nix shell nixpkgs#bash --command bash\n";
        let error = parse(script).expect_err("env -S");
        assert!(error.contains("-S"), "{error}");
        assert!(error.contains("two-line"), "{error}");
    }

    #[test]
    fn a_bare_installable_is_refused_as_unpinned() {
        let script = "#!/usr/bin/env nix\n#! nix shell bash --command bash\n";
        let error = parse(script).expect_err("bare name");
        assert!(error.contains("nixpkgs#bash"), "{error}");
    }

    #[test]
    fn a_non_nixpkgs_flake_reference_is_refused_with_the_lock_reason() {
        let script = "#!/usr/bin/env nix\n\
                      #! nix shell github:owner/repo#tool --command tool\n";
        let error = parse(script).expect_err("foreign flake");
        assert!(error.contains("no Nix"), "{error}");
    }

    #[test]
    fn only_the_shell_subcommand_is_desugared() {
        let script = "#!/usr/bin/env nix\n#! nix develop --command bash\n";
        let error = parse(script).expect_err("develop");
        assert!(error.contains("nix shell"), "{error}");
    }

    #[test]
    fn a_missing_command_is_reported_rather_than_guessed() {
        let script = "#!/usr/bin/env nix\n#! nix shell nixpkgs#bash\n";
        let error = parse(script).expect_err("no --command");
        assert!(error.contains("--command"), "{error}");
    }

    #[test]
    fn quotes_are_refused_because_they_are_not_interpreted() {
        let script = "#!/usr/bin/env nix\n\
                      #! nix shell \"nixpkgs#bash\" --command bash\n";
        let error = parse(script).expect_err("quoted");
        assert!(error.contains("quotes"), "{error}");
    }

    #[test]
    fn an_attribute_path_that_would_not_parse_as_nix_is_refused() {
        // Unquoted deliberately. With quotes the apostrophe trips the earlier
        // quoting check and this never reaches `attribute_path`, which is the
        // thing the name claims is under test.
        let script = "#!/usr/bin/env nix\n\
                      #! nix shell nixpkgs#bash;abort --command bash\n";
        let error = parse(script).expect_err("refused");
        assert!(error.contains("attribute path"), "{error}");
    }

    #[test]
    fn an_attribute_whose_component_is_not_an_identifier_is_still_accepted() {
        // `haskellPackages` really carries `3d-graphics-examples`, `2captcha`
        // and `4Blocks`. Nix lexes a bare `.3` as a float, so these have to be
        // quoted downstream rather than refused here — refusing would reject
        // an installable `nix shell` accepts.
        let script = "#!/usr/bin/env nix\n\
                      #! nix shell nixpkgs#haskellPackages.3d-graphics-examples \
                      nixpkgs#bash --command bash\n";
        let parsed = parse(script).expect("parses");
        assert_eq!(
            parsed.packages,
            ["haskellPackages.3d-graphics-examples", "bash"]
        );
    }

    #[test]
    fn an_interpreter_that_is_a_path_is_refused_rather_than_doubled() {
        // `/bin/` + `/bin/bash` is `/bin//bin/bash`, which resolves to nothing
        // a sparse rootfs provides.
        let script = "#!/usr/bin/env nix\n\
                      #! nix shell nixpkgs#bash --command /bin/bash\n";
        let error = parse(script).expect_err("refused");
        assert!(error.contains("bare binary name"), "{error}");
    }

    #[test]
    fn an_interpreter_that_could_open_a_nix_interpolation_is_refused() {
        let script = "#!/usr/bin/env nix\n\
                      #! nix shell nixpkgs#bash --command ${myShell}\n";
        assert!(parse(script).is_err());
    }

    #[test]
    fn a_dotted_attribute_path_is_kept() {
        let script = "#!/usr/bin/env nix\n\
                      #! nix shell nixpkgs#python3Packages.requests nixpkgs#python3 \
                      --command python3\n";
        let parsed = parse(script).expect("parses");
        assert_eq!(parsed.packages, ["python3Packages.requests", "python3"]);
    }

    #[test]
    fn shebang_detection_requires_nix_to_be_the_program() {
        assert!(is_nix_shebang("#!/usr/bin/env nix\n"));
        assert!(is_nix_shebang("#!/nix/store/abc/bin/nix\n"));
        assert!(!is_nix_shebang("#!/usr/bin/env bash\n"));
        // `nix` as an argument to something else is not a nix shebang.
        assert!(!is_nix_shebang("#!/bin/sh nix\n"));
        assert!(!is_nix_shebang("cowsay hello\n"));
        assert!(!is_nix_shebang(""));
    }
}
