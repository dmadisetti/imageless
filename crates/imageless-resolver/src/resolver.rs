//! The imageless resolver daemon: a UNIX-socket service that materializes
//! releases and development sources for the runc interposer.

use imageless::{load_resolver_policy, serve, DevelopmentWorkerConfig, Resolver, ResolverConfig};
use std::path::PathBuf;

fn usage() -> ! {
    eprintln!(
        "usage: imageless-resolver [--socket-path PATH] [--max-realizations 1..64] \
         [--realization-timeout-seconds 1..3600] [--policy-file PATH] \
         [--development-worker PATH --development-worker-user USER]"
    );
    std::process::exit(2);
}

fn value(args: &mut impl Iterator<Item = String>) -> String {
    args.next().unwrap_or_else(|| usage())
}

fn main() {
    let mut socket_path = PathBuf::from("/run/imageless/resolver.sock");
    let mut max_realizations = 2_usize;
    let mut timeout_seconds = 300_u64;
    let mut policy_file = None;
    let mut development_worker = None;
    let mut development_worker_user = None;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--socket-path" => socket_path = PathBuf::from(value(&mut args)),
            "--max-realizations" => {
                max_realizations = value(&mut args).parse().unwrap_or_else(|_| usage())
            }
            "--realization-timeout-seconds" => {
                timeout_seconds = value(&mut args).parse().unwrap_or_else(|_| usage())
            }
            "--policy-file" => policy_file = Some(PathBuf::from(value(&mut args))),
            "--development-worker" => development_worker = Some(PathBuf::from(value(&mut args))),
            "--development-worker-user" => development_worker_user = Some(value(&mut args)),
            "--help" | "-h" => usage(),
            _ => usage(),
        }
    }
    if !(1..=64).contains(&max_realizations) || !(1..=3600).contains(&timeout_seconds) {
        usage();
    }

    let mut config = ResolverConfig::from_environment(max_realizations, timeout_seconds);
    if let Some(path) = policy_file {
        config.policy = load_resolver_policy(&path).unwrap_or_else(|error| {
            eprintln!(
                "imageless-resolver: load policy {}: {error}",
                path.display()
            );
            std::process::exit(1);
        });
    }
    config.development_worker = match (development_worker, development_worker_user) {
        (Some(program), Some(user)) if !user.is_empty() => {
            Some(DevelopmentWorkerConfig { program, user })
        }
        (None, None) => None,
        _ => usage(),
    };
    if !config.policy.cache_only && config.development_worker.is_none() {
        eprintln!(
            "imageless-resolver: evaluation mode (cache_only = false) requires --development-worker and --development-worker-user"
        );
        std::process::exit(1);
    }
    let resolver = Resolver::new(config);
    if let Err(error) = serve(&socket_path, resolver) {
        eprintln!("imageless-resolver: {error}");
        std::process::exit(1);
    }
}
