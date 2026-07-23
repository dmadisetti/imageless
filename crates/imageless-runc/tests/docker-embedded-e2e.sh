set -euo pipefail

archive=${IMAGELESS_DOCKER_IMAGE_ARCHIVE:?IMAGELESS_DOCKER_IMAGE_ARCHIVE is required}
image=${IMAGELESS_DOCKER_IMAGE:-localhost/imageless-docker-embedded:e2e}
runtime=${IMAGELESS_DOCKER_RUNTIME:-imageless}
network=${IMAGELESS_DOCKER_NETWORK:-bridge}
expected=${IMAGELESS_DOCKER_EXPECTED:-imageless-docker-embedded-ok}
# Extra `docker run` arguments, e.g. `--tmpfs /tmp` for a service that needs a
# writable scratch dir under the readonly materialized root (nginx).
read -r -a extra_run_args <<<"${IMAGELESS_DOCKER_RUN_ARGS:-}"
container=''

cleanup() {
  if [[ -n $container ]]; then
    docker rm -f "$container" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

docker info >/dev/null
docker load --input "$archive" >/dev/null

if [[ $network == host ]]; then
  container=$(docker run --runtime "$runtime" --network host --detach \
    "${extra_run_args[@]}" "$image")
  port=18080
elif [[ $network == none ]]; then
  container=$(docker run --runtime "$runtime" --network none --detach \
    "${extra_run_args[@]}" "$image")
  port=''
else
  container=$(docker run --runtime "$runtime" --detach --publish 127.0.0.1::18080 \
    "${extra_run_args[@]}" "$image")
  port=$(docker port "$container" 18080/tcp | sed -n 's/.*:\([0-9][0-9]*\)$/\1/p' | head -n 1)
  [[ -n $port ]]
fi

response=''
for _ in $(seq 1 100); do
  if [[ $network == none ]]; then
    response=$(docker exec "$container" /bin/busybox wget -qO- http://127.0.0.1:18080/ 2>/dev/null || true)
  else
    response=$(curl --fail --silent --show-error "http://127.0.0.1:$port/" 2>/dev/null || true)
  fi
  if [[ $response == "$expected" ]]; then
    break
  fi
  if [[ $(docker inspect "$container" --format '{{.State.Running}}') != true ]]; then
    docker logs "$container" >&2 || true
    echo "embedded-source Docker workload exited before readiness" >&2
    exit 1
  fi
  sleep 0.05
done

[[ $response == "$expected" ]]
echo "$response"
