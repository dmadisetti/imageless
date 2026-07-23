{
  # Ship the flake, not the filesystem. This image layer carries ONLY this
  # flake and its lock — no nginx, no closure, a few kilobytes. The imageless
  # node evaluates `#rootfs` at container-create, materializes nginx from its
  # own cache, and binds /nix/store read-only so the sparse root's dynamic
  # loader resolves. `docker run --runtime imageless` on this image serves
  # nginx; a stock runtime cannot, because /bin/nginx does not exist until the
  # node materializes the embedded flake.
  #
  # Pinned to the same nixpkgs the imageless repo itself locks, so a node that
  # already has that input (any imageless build host) resolves it offline.
  inputs.nixpkgs.url = "github:nixos/nixpkgs/8c3cede7ddc26bd659d2d383b5610efbd2c7a16e";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      # nginx needs writable pid/temp dirs under the readonly materialized
      # root; point them all at the container's tmpfs /tmp. Serves a fixed
      # sentinel so the e2e can assert an exact body.
      nginxConf = pkgs.writeText "imageless-nginx.conf" ''
        worker_processes 1;
        master_process off;
        daemon off;
        pid /tmp/nginx.pid;
        error_log /proc/self/fd/2 info;
        events { worker_connections 64; }
        http {
          access_log /proc/self/fd/1;
          client_body_temp_path /tmp/client_body;
          proxy_temp_path /tmp/proxy;
          fastcgi_temp_path /tmp/fastcgi;
          uwsgi_temp_path /tmp/uwsgi;
          scgi_temp_path /tmp/scgi;
          server {
            listen 18080;
            location / { default_type text/plain; return 200 "imageless-nginx-ok\n"; }
          }
        }
      '';
    in
    {
      # SPARSE: nginx and its loader stay in /nix/store. The node's store
      # projection (StoreProjection::Node binds the whole store; Closure binds
      # just this closure) supplies them at container-create.
      rootfs = pkgs.runCommand "imageless-nginx-embedded-rootfs" { } ''
        mkdir -p $out/bin $out/dev $out/etc/nginx $out/nix/store $out/proc \
          $out/sys $out/tmp $out/var/log/nginx
        ln -s ${pkgs.nginx}/bin/nginx $out/bin/nginx
        ln -s ${nginxConf} $out/etc/nginx/nginx.conf
        # nginx runs as root here and drops workers to its compile-time default
        # user (nobody); getpwnam must resolve or startup is fatal. Ship both.
        printf 'root:x:0:0:root:/tmp:/bin/nginx\nnobody:x:65534:65534:nobody:/tmp:/bin/nginx\n' > $out/etc/passwd
        printf 'root:x:0:\nnogroup:x:65534:\n' > $out/etc/group
        # CRI/Docker bind hostname/hosts/resolv.conf over the readonly root;
        # the mountpoints must already exist.
        touch $out/etc/hostname $out/etc/hosts $out/etc/resolv.conf
      '';
    };
}
