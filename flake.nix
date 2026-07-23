{
  description = "imageless — ship the flake, not the filesystem: OCI images that materialize their rootfs from an embedded Nix flake at container-create time";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/8c3cede7ddc26bd659d2d383b5610efbd2c7a16e";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);
    in
    {
      nixosModules.imageless = { pkgs, lib, ... }: {
        imports = [ ./nix/module.nix ];
        services.imageless.package =
          lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.imageless;
      };

      overlays.default = final: _prev: {
        imageless = final.callPackage ./nix/package.nix { };
      };

      # Full per-system set, INCLUDING the two NixOS VM acceptance gates. This
      # is not a standard flake output name, so `nix flake check` never
      # evaluates it — `packages` (below) drops the VM gates and `legacyPackages`
      # re-exposes them without duplicating their definitions.
      allPackages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          inherit (pkgs) lib;

          # The whole workspace in one package: the shim, the optional
          # resolver daemon, and the development evaluator. The NixOS module
          # points every consumer here.
          imageless = pkgs.callPackage ./nix/package.nix { };

          # Same workspace built with the `inline-policy` feature, for the dev
          # Docker harness only (a root daemon reads policy from
          # `IMAGELESS_POLICY_JSON` — see dev/docker/README.md). Never deploy it
          # to a shared node: production `.#imageless` cannot read env policy.
          imageless-dev = pkgs.callPackage ./nix/package.nix { inlinePolicy = true; };

          # Single-binary views for the quick starts (`nix build .#imageless-runc`).
          binView = name: pkgs.runCommand name
            { meta.mainProgram = name; }
            ''
              mkdir -p $out/bin
              ln -s ${imageless}/bin/${name} $out/bin/${name}
            '';
          imageless-runc = binView "imageless-runc";
          imageless-resolver = binView "imageless-resolver";

          # -- Docker embedded-layer acceptance gate (SPEC.md §7.1) ---------
          # The seed carries ONLY the flake and its inputs: no metadata file,
          # no labels — presence of etc/imageless/flake.nix is the contract.
          # The seed's top level deliberately has no /bin/busybox, so the
          # workload cannot pass by running unchanged.
          docker-embedded-seed = pkgs.runCommand "imageless-docker-embedded-seed" { } ''
            root=$out/etc/imageless/rootfs
            mkdir -p $root/bin $root/dev $root/etc $root/nix/store $root/proc \
              $root/sys $root/tmp $root/www
            cp ${pkgs.pkgsStatic.busybox}/bin/busybox $root/bin/busybox
            printf 'imageless-docker-embedded-ok\n' > $root/www/index.html
            printf 'root:x:0:0:root:/tmp:/bin/busybox\n' > $root/etc/passwd
            printf 'root:x:0:\n' > $root/etc/group
            touch $root/etc/hostname $root/etc/hosts $root/etc/resolv.conf
            printf '%s\n' '{' \
              '  outputs = { self }: {' \
              '    rootfs = builtins.path {' \
              '      path = ./rootfs;' \
              '      name = "imageless-docker-embedded-rootfs";' \
              '    };' \
              '  };' \
              '}' > $out/etc/imageless/flake.nix
          '';
          docker-embedded-image = pkgs.dockerTools.buildImage {
            name = "localhost/imageless-docker-embedded";
            tag = "e2e";
            copyToRoot = docker-embedded-seed;
            config = {
              Entrypoint = [ "/bin/busybox" "httpd" "-f" "-p" "18080" "-h" "/www" ];
              ExposedPorts."18080/tcp" = { };
            };
          };

          # A real dynamically-linked service (nginx) shipped the imageless
          # way. The seed carries ONLY examples/nginx-embedded/{flake.nix,
          # flake.lock} — no nginx, no closure, a few kilobytes with ZERO
          # /nix/store references (so dockerTools cannot drag a closure into
          # the layer). The node evaluates the embedded flake's SPARSE `#rootfs`
          # at container-create, materializes nginx from its own cache, and
          # binds /nix/store read-only (StoreProjection::Node) so the loader
          # resolves. This exercises the true late-bound path — unlike the
          # static-busybox seed above, which ships its own filesystem. nginx
          # needs writable pid/temp dirs under the readonly root, so run it with
          # `--tmpfs /tmp`.
          nginx-embedded-seed = pkgs.runCommand "imageless-nginx-embedded-seed" { } ''
            mkdir -p $out/etc/imageless
            cp ${./examples/nginx-embedded/flake.nix} $out/etc/imageless/flake.nix
            cp ${./examples/nginx-embedded/flake.lock} $out/etc/imageless/flake.lock
          '';
          nginx-embedded-image = pkgs.dockerTools.buildImage {
            name = "localhost/imageless-nginx-embedded";
            tag = "e2e";
            copyToRoot = nginx-embedded-seed;
            config = {
              # -e redirects the compile-time default error log (opened before
              # the config's error_log directive applies) off the read-only
              # /var/log/nginx path to the container's stderr.
              Entrypoint = [ "/bin/nginx" "-e" "/proc/self/fd/2" "-c" "/etc/nginx/nginx.conf" ];
              ExposedPorts."18080/tcp" = { };
            };
          };
          # The tiny CRI placeholder for release-profile pods
          # (examples/pod-imageless.yaml): the release manifest supplies the
          # real rootfs and process metadata, the image is only a pull target.
          placeholder-image = pkgs.dockerTools.buildImage {
            name = "localhost/imageless-placeholder";
            tag = "v1";
            config.Cmd = [ "/imageless-placeholder" ];
          };
          # An ordinary image with no embedded flake: must pass through the
          # imageless runtime completely unchanged.
          docker-passthrough-image = pkgs.dockerTools.buildImage {
            name = "localhost/imageless-passthrough";
            tag = "e2e";
            copyToRoot = pkgs.runCommand "imageless-passthrough-root" { } ''
              mkdir -p $out/bin
              cp ${pkgs.pkgsStatic.busybox}/bin/busybox $out/bin/busybox
              ln -s busybox $out/bin/true
            '';
            config.Cmd = [ "/bin/true" ];
          };
          docker-embedded-scenario = pkgs.writeShellApplication {
            name = "imageless-docker-embedded-scenario";
            runtimeInputs = [ pkgs.coreutils pkgs.curl pkgs.docker-client pkgs.gnused pkgs.jq ];
            text = ''
              exec bash ${./crates/imageless-runc/tests/docker-embedded-e2e.sh} "$@"
            '';
          };
          # Hermetic VM proof of the DAEMONLESS path: raw Docker, the shim
          # registered as a Docker runtime, node policy from
          # /etc/imageless/policy.json, and no resolver daemon anywhere in
          # the VM. Materialization happens in-process inside imageless-runc.
          docker-embedded-smoke = pkgs.testers.runNixOSTest {
            name = "imageless-docker-embedded";
            nodes.node = { ... }: {
              imports = [ self.nixosModules.imageless ];
              services.imageless = {
                enable = true;
                # Docker is wired by hand below; keep containerd out of it.
                containerd.enable = false;
                policy.cacheOnly = false;
                policy.evalAllowedUriPrefixes = [ "path:" ];
              };
              virtualisation.docker = {
                enable = true;
                daemon.settings.runtimes.imageless.path =
                  "${imageless}/bin/imageless-runc";
              };
              virtualisation = {
                writableStore = true;
                memorySize = 3072;
                diskSize = 4096;
                cores = 2;
              };
              environment.systemPackages = [ docker-embedded-scenario ];
            };
            testScript = ''
              node.wait_for_unit("docker.service")

              # No daemon in this VM: the socket the daemon profile would use
              # must not exist, so a passing smoke proves the in-process path.
              node.fail("test -e /run/imageless/resolver.sock")

              # An ordinary image passes through unchanged.
              node.succeed("docker load < ${docker-passthrough-image}")
              node.succeed(
                  "docker run --rm --runtime imageless --network none "
                  "localhost/imageless-passthrough:e2e"
              )

              # The seed's layer lacks the executable that produces the
              # response; serving it proves the rootfs was materialized from
              # the embedded flake.
              print(node.succeed(
                  "IMAGELESS_DOCKER_IMAGE_ARCHIVE=${docker-embedded-image} "
                  "IMAGELESS_DOCKER_NETWORK=none "
                  "imageless-docker-embedded-scenario 2>&1"
              ))
            '';
          };

          # -- CRI lifecycle acceptance gate (SPEC.md §7.2) -----------------
          smoke-image = pkgs.dockerTools.buildImage {
            name = "localhost/imageless-smoke";
            tag = "phase0";
            copyToRoot = pkgs.buildEnv {
              name = "imageless-smoke-root";
              paths = [ pkgs.busybox ];
              pathsToLink = [ "/bin" ];
            };
            config.Cmd = [ "/bin/sleep" "600" ];
          };
          smoke-rootfs = pkgs.runCommand "imageless-smoke-rootfs" { } ''
            mkdir -p $out/bin $out/etc $out/tmp $out/nix/store $out/proc $out/sys \
              $out/dev/shm
            ln -s ${pkgs.bash}/bin/bash $out/bin/sh
            ln -s ${pkgs.coreutils}/bin/sleep $out/bin/sleep
            # CRI bind-mounts the sandbox hostname/hosts/resolv.conf (and shm)
            # over the rootfs; the store path is read-only, so the mountpoints
            # must already exist in the materialized root.
            touch $out/etc/hostname $out/etc/hosts $out/etc/resolv.conf
          '';
          smoke-release = pkgs.callPackage ./nix/release-catalog.nix {
            rootfsTargets.${system} = smoke-rootfs;
            issuer = "imageless-smoke";
            releaseName = "cri";
            cache = "local";
          };
          imageless-cri-smoke = pkgs.writeShellApplication {
            name = "imageless-cri-smoke";
            runtimeInputs = [
              pkgs.coreutils
              pkgs.containerd
              pkgs.cri-tools
              pkgs.findutils
              pkgs.gnugrep
              pkgs.jq
              pkgs.nix
              pkgs.util-linux
            ];
            text = ''
              export IMAGELESS_SMOKE_IMAGE_ARCHIVE=${smoke-image}
              # A bare path string, context discarded on purpose: this only
              # names the .drv the smoke realizes in-guest. The .drv (and its
              # input closure) is seeded into the guest store by the VM test's
              # virtualisation.additionalPaths; carrying context here would make
              # imageless-cri-smoke depend on the .drv and drag it into
              # `nix flake check` eval, which cannot instantiate it on a fresh
              # store.
              export IMAGELESS_SMOKE_ROOTFS_DRV=${builtins.unsafeDiscardStringContext smoke-rootfs.drvPath}
              export IMAGELESS_SMOKE_RELEASE_REFERENCE=${smoke-release.reference}
              ${builtins.readFile ./smoke/imageless-cri-smoke.sh}
            '';
          };
          # A real containerd/CRI node driven through the full lifecycle —
          # sandbox passthrough, per-container selection, recreate,
          # GC-while-running, delete-and-collect, reboot recovery — on BOTH
          # store-projection backends. This gate runs the resolver-daemon
          # profile, so CI covers both materializer modes (the Docker gate
          # above covers in-process).
          imageless-cri-vm = pkgs.testers.runNixOSTest (
            let
              nodeBase = { ... }: {
                imports = [ self.nixosModules.imageless ];
                services.imageless = {
                  enable = true;
                  resolver.enable = true;
                  policy.issuers.imageless-smoke = {
                    source = {
                      kind = "local";
                      directory = smoke-release;
                    };
                    allowedReleases = [ "cri" ];
                    caches.local = {
                      substituter = "file:///nix/store";
                      publicKeys = [ ];
                    };
                  };
                };

                # The VM is hermetic (no network). Point the pinned
                # pod-sandbox ("pause") image at the locally-imported smoke
                # image so RunPodSandbox resolves locally instead of pulling
                # the containerd default from registry.k8s.io.
                virtualisation.containerd.settings.plugins."io.containerd.cri.v1.images".pinned_images.sandbox =
                  "localhost/imageless-smoke:phase0";

                # The smoke realises and GCs the disposable rootfs INSIDE the
                # guest and asserts GC reclaims it after reboot. That only
                # holds if the rootfs is built in-guest into the writable
                # layer — so seed its derivation and build inputs, never its
                # output.
                virtualisation = {
                  writableStore = true;
                  memorySize = 4096;
                  cores = 2;
                  diskSize = 8192;
                  additionalPaths = [
                    smoke-image
                    imageless-cri-smoke
                    smoke-rootfs.drvPath
                    pkgs.bash
                    pkgs.coreutils
                    pkgs.stdenvNoCC
                  ];
                };

                # `jq` for the projection-shape assertions in the testScript.
                environment.systemPackages = [ imageless-cri-smoke pkgs.jq ];
              };
            in
            {
              name = "imageless-cri-lifecycle";
              nodes.node = nodeBase;
              nodes.closure = { ... }: {
                imports = [ nodeBase ];
                services.imageless.storeProjection = "closure";
              };
              testScript = ''
                for machine in (node, closure):
                    machine.wait_for_unit("imageless-resolver.service")
                    machine.wait_for_unit("containerd.service")

                def rewritten_bundle(machine):
                    # The retained witness container from `pre-reboot` is
                    # still running; its bundle is the one carrying the
                    # imageless GC root (the pod sandbox is never rewritten).
                    return machine.succeed(
                        "dirname \"$(find "
                        "/run/containerd/io.containerd.runtime.v2.task/k8s.io "
                        "-name .imageless-rootfs-gcroot | head -1)\""
                    ).strip()

                def run_smoke(machine, phase):
                    # On failure the smoke's EXIT trap tears the pod down, so
                    # bundle dirs vanish — but the resolver journal and the
                    # telemetry sink persist and are the decisive signal for
                    # whether the shim ran per-container resolution at all.
                    status, output = machine.execute(
                        "imageless-cri-smoke " + phase + " 2>&1"
                    )
                    print(output)
                    if status != 0:
                        print("### DIAG " + machine.name + " " + phase + " ###")
                        print("--- imageless-resolver journal ---")
                        print(machine.execute(
                            "journalctl -u imageless-resolver --no-pager "
                            "--no-hostname | tail -n 80"
                        )[1])
                        print("--- resolver timings (/run/imageless/timings.jsonl) ---")
                        print(machine.execute(
                            "cat /run/imageless/timings.jsonl 2>&1"
                        )[1])
                        print("--- containerd journal (shim/imageless) ---")
                        print(machine.execute(
                            "journalctl -u containerd --no-pager --no-hostname | "
                            "grep -iE 'imageless|shim start|bundle selection|"
                            "gcroot|resolve|runc.v2' | tail -n 60"
                        )[1])
                        print("--- live task bundles ---")
                        print(machine.execute(
                            "ls -laR /run/containerd/io.containerd.runtime.v2.task"
                            "/k8s.io/ 2>&1 | tail -n 80"
                        )[1])
                        raise Exception(
                            machine.name + " imageless-cri-smoke " + phase + " failed"
                        )

                # Full pre-reboot lifecycle on each backend: annotated
                # sandbox, selected init, unselected sidecar, main, live-GC
                # survival, CRI recreate, failed start, delete, then a
                # retained witness workload.
                run_smoke(node, "pre-reboot")
                run_smoke(closure, "pre-reboot")

                # The node backend binds the whole store once and no
                # per-path store mounts.
                node.succeed(
                    "jq -e '([.mounts[]|select(.destination==\"/nix/store\")]|length==1) "
                    "and ([.mounts[]|select(.destination|startswith(\"/nix/store/\"))]|length==0)' "
                    + rewritten_bundle(node) + "/config.json"
                )
                # The closure backend binds only per-path closure mounts, lays a
                # single empty tmpfs over /nix/store so those mountpoints can be
                # created in the read-only rootfs, and never binds the whole node
                # store.
                closure.succeed(
                    "jq -e '([.mounts[]|select(.destination|startswith(\"/nix/store/\"))]|length>0) "
                    "and ([.mounts[]|select(.destination==\"/nix/store\" and .type==\"tmpfs\")]|length==1) "
                    "and ([.mounts[]|select(.destination==\"/nix/store\" and .type==\"bind\")]|length==0)' "
                    + rewritten_bundle(closure) + "/config.json"
                )

                for machine in (node, closure):
                    machine.reboot()
                    machine.wait_for_unit("containerd.service")
                    machine.wait_for_unit("imageless-resolver.service")

                # Post-reboot on each backend: the /run bundle GC root is
                # gone (tmpfs), GC reclaims the now-unrooted disposable
                # rootfs, and a fresh workload still resolves.
                run_smoke(node, "post-reboot")
                run_smoke(closure, "post-reboot")
              '';
            }
          );

          # Store-projection shape proof against completely stock runc: a
          # materialized-style rootfs whose binaries live in the node store
          # runs under an unmodified rootless runc with a single read-only
          # store bind.
          stock-oci-smoke = pkgs.writeShellApplication {
            name = "imageless-stock-oci-smoke";
            runtimeInputs = [ pkgs.binutils pkgs.coreutils pkgs.gnused pkgs.jq ];
            text = ''
              exec bash ${./crates/imageless-runc/tests/stock-runc.sh} \
                ${pkgs.runc}/bin/runc \
                ${smoke-rootfs}
            '';
          };
        in
        {
          inherit imageless imageless-dev imageless-runc imageless-resolver;
          inherit docker-embedded-seed docker-embedded-image docker-passthrough-image placeholder-image;
          inherit nginx-embedded-seed nginx-embedded-image;
          inherit docker-embedded-scenario;
          inherit docker-embedded-smoke imageless-cri-vm;
          inherit smoke-image smoke-rootfs smoke-release imageless-cri-smoke;
          inherit stock-oci-smoke;
          default = imageless-runc;
        });

      # `nix flake check` evaluates every derivation under `packages` and
      # `checks`. A NixOS VM test's qemu closure reads the .drv it seeds via
      # virtualisation.additionalPaths (smoke-rootfs) at EVAL time, which is not
      # valid on a fresh store — so the two VM gates cannot live under either.
      # They go under legacyPackages, the one derivation-bearing output flake
      # check skips. `nix build .#imageless-cri-vm` still resolves them (the
      # installable lookup falls through packages -> legacyPackages), and the
      # KVM acceptance-gates CI job builds them there.
      packages = forAllSystems (system:
        builtins.removeAttrs self.allPackages.${system} [
          "docker-embedded-smoke"
          "imageless-cri-vm"
        ]);

      legacyPackages = forAllSystems (system: {
        inherit (self.allPackages.${system}) docker-embedded-smoke imageless-cri-vm;
      });

      checks = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          inherit (pkgs) lib;
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./crates ];
          };
          # Workspace-wide gate: fails on any rustfmt drift or clippy warning.
          lint = pkgs.stdenv.mkDerivation {
            name = "imageless-lint";
            inherit src;
            cargoDeps = pkgs.rustPlatform.importCargoLock { lockFile = ./Cargo.lock; };
            nativeBuildInputs = [
              pkgs.rustPlatform.cargoSetupHook
              pkgs.cargo
              pkgs.rustc
              pkgs.clippy
              pkgs.rustfmt
            ];
            buildPhase = ''
              export HOME=$TMPDIR
              cargo fmt --check
              cargo clippy --workspace --all-targets --offline -- -D warnings
            '';
            installPhase = "touch $out";
          };
          # Evaluate the NixOS module in both materializer modes and assert
          # the wiring it promises, so module regressions fail eval — not a
          # node.
          module-eval =
            let
              evalNode = extra:
                (nixpkgs.lib.nixosSystem {
                  modules = [
                    self.nixosModules.imageless
                    {
                      nixpkgs.hostPlatform = system;
                      boot.loader.grub.enable = false;
                      fileSystems."/" = { device = "none"; fsType = "tmpfs"; };
                      system.stateVersion = "25.05";
                    }
                    extra
                  ];
                }).config;
              daemonless = evalNode {
                services.imageless.enable = true;
              };
              daemon = evalNode {
                services.imageless = {
                  enable = true;
                  resolver.enable = true;
                  policy.cacheOnly = false;
                  policy.evalAllowedUriPrefixes = [ "path:" ];
                };
              };
              runtime = daemonless.virtualisation.containerd.settings.plugins."io.containerd.grpc.v1.cri".containerd.runtimes.imageless;
            in
            assert lib.hasSuffix "/bin/imageless-runc" runtime.options.BinaryName;
            assert runtime.runtime_type == "io.containerd.runc.v2";
            assert runtime.pod_annotations == [ "imageless.run/*" "run.imageless.*" ];
            assert runtime.container_annotations == [ "imageless.run/*" "run.imageless.*" ];
            # Daemonless: no socket in containerd's environment, no resolver
            # unit, policy staged at the in-process default path.
            assert !(daemonless.systemd.services.containerd.environment ? IMAGELESS_RESOLVER_SOCKET);
            assert !(daemonless.systemd.services ? imageless-resolver)
              || daemonless.systemd.services.imageless-resolver.enable == false;
            # Policy is a real copied file at the in-process default path —
            # the loader fail-closed rejects environment.etc symlinks.
            assert lib.any (lib.hasPrefix "C+ /etc/imageless/policy.json")
              daemonless.systemd.tmpfiles.rules;
            # Daemon profile: socket exported, resolver runs with the
            # unprivileged development worker when evaluation is allowed.
            assert daemon.systemd.services.containerd.environment.IMAGELESS_RESOLVER_SOCKET
              == "/run/imageless/resolver.sock";
            assert lib.hasInfix "--development-worker"
              daemon.systemd.services.imageless-resolver.serviceConfig.ExecStart;
            assert daemon.users.users ? imageless-dev;
            pkgs.writeText "imageless-module-eval" "ok";
        in
        # Every package is a check (a package that doesn't build is a broken
        # release), plus the lint/module-eval gates only checks carry. The two
        # NixOS VM gates are deliberately absent from `packages` (they live under
        # legacyPackages — see above), so they are naturally excluded here too.
        self.packages.${system} // {
          inherit lint module-eval;
        });

      devShells = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.rustfmt
              pkgs.clippy
              pkgs.rust-analyzer
              pkgs.jq
              pkgs.nixpkgs-fmt
            ];
          };
        });

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixpkgs-fmt);
    };
}
