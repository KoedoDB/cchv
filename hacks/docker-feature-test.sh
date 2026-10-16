#!/bin/sh
# Exercise the Docker features that matter against the cloudhv runtime and print what each one does. Run it where `docker` talks to a
# Docker that has the runtime (hacks/try-with-docker.sh sets that up: `docker exec cchv-dind /hacks/docker-feature-test.sh`). Not a pass/fail
# test: read the output; docs/try-with-docker.md says what is expected.
R="--runtime=cloudhv"
t() { echo "=== $1"; }
t "exit code propagates (exit 3)"; docker run --rm $R alpine:3.19 sh -c 'exit 3'; echo "exit=$?"
t "stdout and stderr"; docker run --rm $R alpine:3.19 sh -c 'echo to-out; echo to-err >&2' 2>/tmp/err.txt; echo "stderr was: $(cat /tmp/err.txt)"
t "-m 512m --cpus 2 sizes the VM"; docker run --rm $R -m 512m --cpus 2 alpine:3.19 sh -c 'head -1 /proc/meminfo; nproc'
t "stdin with -i"; echo piped-input | docker run --rm -i $R alpine:3.19 cat
t "-t (a tty)"; docker run --rm -t $R alpine:3.19 sh -c 'tty; echo tty-ok' | cat -v | head -3
t "detached + logs + exec + ps"
docker rm -f web >/dev/null 2>&1
docker run -d --name web $R -p 8080:80 nginx:1.27-alpine >/dev/null; sleep 3
echo "logs: $(docker logs web 2>&1 | head -2 | tr '\n' '|' | cut -c1-120)"
echo "exec: $(docker exec web sh -c 'echo in-container; id -u')"
echo "ps: $(docker ps --format '{{.Names}} {{.Status}} {{.Ports}}')"
echo "published port 8080 -> VM: $(wget -q -O - -T 5 http://127.0.0.1:8080/ 2>&1 | head -c 40)"
echo "container IP direct: $(wget -q -O - -T 5 http://$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' web)/ 2>&1 | head -c 40)"
echo "inspect pid: $(docker inspect -f '{{.State.Pid}}' web)"
echo "top: $(docker top web 2>&1 | head -3 | tr '\n' '|' | cut -c1-150)"
echo "stats: $(timeout 8 docker stats --no-stream web 2>&1 | tail -2 | tr '\n' '|' | cut -c1-200)"
t "stop (SIGTERM) and exit status"; s=$(date +%s); docker stop web >/dev/null; echo "stopped in $(( $(date +%s)-s )) s; status: $(docker inspect -f '{{.State.Status}} exit={{.State.ExitCode}}' web)"
docker rm web >/dev/null 2>&1
t "kill -s KILL"; docker run -d --name k1 $R alpine:3.19 sleep 300 >/dev/null; sleep 1; docker kill k1 >/dev/null; sleep 1; docker inspect -f '{{.State.Status}} exit={{.State.ExitCode}}' k1; docker rm k1 >/dev/null
t "--network none"; docker run --rm --network none $R alpine:3.19 sh -c 'ip -o link | cut -d: -f2 | tr "\n" " "; echo; wget -q -O /dev/null -T 3 http://1.1.1.1 && echo net-up || echo no-network'
t "internet and DNS from the VM"; docker run --rm $R alpine:3.19 sh -c 'nslookup github.com 2>&1 | grep -c Address; wget -q -O /dev/null -T 8 https://github.com && echo https-ok || echo https-fail'
t "-v bind of a small dir"; mkdir -p /tmp/hostdir && echo from-host > /tmp/hostdir/f.txt; docker run --rm $R -v /tmp/hostdir:/data alpine:3.19 cat /data/f.txt 2>&1 | tail -2
t "named volume"; docker volume create vol1 >/dev/null; docker run --rm $R -v vol1:/v alpine:3.19 sh -c 'echo x > /v/a; cat /v/a' 2>&1 | tail -2
t "env and workdir and user"; docker run --rm $R -e FOO=bar -w /tmp -u 1000:1000 alpine:3.19 sh -c 'echo $FOO $(pwd) $(id -u)'
t "restart policy on exit"; docker run -d --name rp $R --restart=on-failure:2 alpine:3.19 sh -c 'echo run; exit 1' >/dev/null; sleep 8; echo "restarts: $(docker inspect -f '{{.RestartCount}}' rp)"; docker rm -f rp >/dev/null
t "leftovers"; sleep 3; echo "Cloud Hypervisor processes (3 are the daemon's pool): $(ps -o args | grep -c '[c]loud-hypervisor')"
