# ontele

Helm chart (Helm ≥ 4.0, `apiVersion: v2`, tested with Helm 4.2) for the
Ontele media server and everything it leans on:

| Component | Default | Purpose |
|---|---|---|
| ontele | on | the server (Deployment, Recreate, PVCs for data + recordings, library mounts) |
| postgresql | on | single-node PostgreSQL 16 StatefulSet (or point at `externalDatabase`) |
| oauth2-proxy | on | identity front door; the only thing the Ingress/HTTPRoute exposes |
| dex | **off** | bundled OpenID Connect provider for trials (static users) |
| loki + promtail | on | logs (Promtail DaemonSet tails every pod in the namespace) |
| prometheus | on | scrapes `/metrics` (or use `metrics.serviceMonitor` with prometheus-operator) |
| grafana | on | served at `/grafana/` behind the same login, dashboards provisioned |

## Install

From the published repository (GitHub Pages, branch `helm`):

```bash
helm repo add ontele https://ontele.github.io/ontele
helm install ontele ontele/ontele -n media --create-namespace -f my-values.yaml
```

Releases are cut by `.github/workflows/helm-release.yml` whenever
`deploy/helm/**` changes on `main` — bump `version:` in `Chart.yaml` to
publish (versions are immutable). From a checkout:

```bash
# trial: bundled Dex users admin@example.com / viewer@example.com (password "password")
helm install ontele deploy/helm/ontele -n media --create-namespace \
  --set ingress.host=ontele.example.com \
  --set persistence.media.hostPath=/tank/media \
  --set dex.enabled=true

# production: your OIDC provider, secrets from a pre-created Secret
kubectl -n media create secret generic ontele-oauth \
  --from-literal=OAUTH2_PROXY_CLIENT_ID=… --from-literal=OAUTH2_PROXY_CLIENT_SECRET=… \
  --from-literal=OAUTH2_PROXY_COOKIE_SECRET=$(python3 -c 'import os,base64;print(base64.urlsafe_b64encode(os.urandom(32)).decode())')
helm install ontele deploy/helm/ontele -n media --create-namespace -f my-values.yaml
helm test ontele -n media
```

`my-values.yaml`:

```yaml
ingress:
  host: ontele.example.com
  tls: { enabled: true, secretName: ontele-tls }
config:
  adminUsers: you@example.com
  hdhrIp: 192.168.1.50           # or hostNetwork: true for UDP discovery
  tmdbApiKey: "…"
persistence:
  media: { existingClaim: nas-media }          # or hostPath / nfs
  music: { enabled: true, nfs: { server: nas, path: /music } }
  recordings: { size: 2Ti, storageClass: fast }
oauth2Proxy:
  oidcIssuerUrl: https://login.example.com/realms/home
  existingSecret: ontele-oauth
```

## Values

Every key is documented inline in [values.yaml](values.yaml) and validated by
[values.schema.json](values.schema.json) (unknown keys and bad enums fail at
install time). Highlights:

| Key | Default | Notes |
|---|---|---|
| `config.auth` | `proxy` | `none` exposes Ontele without identity — LAN only |
| `config.adminUsers` / `adminGroups` | `""` | empty ⇒ first user to sign in becomes admin |
| `config.mediaDirs` | `/media/movies,/media/tv` | scan roots in the media share — the folder decides the kind; siblings under /media are ignored |
| `config.dvrPostCmd` | `""` | post-process finished recordings — e.g. `/usr/local/bin/handbrake-postprocess.sh` (encode + move into the library) |
| `replicaCount` / `autoscaling` | `1` / off | ontele & dex are StatefulSets (stable pod names `ontele-0`, `ontele-dex-0`); HPAs scale them — >1 ontele replica needs RWX storage |
| `handbrake.enabled` | `false` | jlesage/handbrake companion (web UI :5800) with watch/output/storage mounts |
| `config.existingSecret` | `""` | Secret with `DATABASE_URL` (external DB) and `ONTELE_TMDB_API_KEY` |
| `hostNetwork` | `false` | needed for HDHomeRun UDP discovery |
| `dri.enabled` | `false` | mounts `/dev/dri` for VA-API/QSV transcoding |
| `global.storageClass` | `""` | default for every PVC; per-volume `storageClass` overrides; `"-"` = static binding |
| `persistence.media` | auto-created PVC (250Gi) | or point at what you have: `existingClaim` / `hostPath` / `nfs` (those skip the PVC) |
| `postgresql.auth.password` | generated | generated once, kept across upgrades (`helm.sh/resource-policy: keep`) |
| `externalDatabase.url` / `existingSecret` | | used when `postgresql.enabled=false` |
| `oauth2Proxy.existingSecret` | `""` | keys `OAUTH2_PROXY_CLIENT_ID`, `_CLIENT_SECRET`, `_COOKIE_SECRET` |
| `oauth2Proxy.redirectUrl` | `<scheme>://<host>/oauth2/callback` | derived from `ingress`/`httpRoute` |
| `dex.enabled` | `false` | replaces `oauth2Proxy.oidcIssuerUrl`; routes `/dex` on the edge |
| `ingress` / `httpRoute` | Ingress on | both can be enabled; TLS decides cookie security and URL scheme |
| `gateway.enabled` | `false` | in-chart Gateway API `Gateway` (`className: cilium`); renders the HTTPRoute too |
| `oauth2Proxy.service.type` | `ClusterIP` | `NodePort` (+ `nodePort` pin) exposes the authenticated front door with no LB/Ingress |
| `networkPolicy.enabled` | `true` | only oauth2-proxy + Prometheus may reach Ontele |
| `metrics.serviceMonitor.enabled` | `false` | prometheus-operator integration |
| `promtail.lokiUrl` | in-chart Loki | your Loki push endpoint; **required** when `loki.enabled=false` |
| `grafana.sidecar.enabled` | `false` | with `grafana.enabled=false`: publish dashboard/datasource ConfigMaps for an existing Grafana's sidecar |

## Storage

Every PVC honours `global.storageClass` (per-volume `storageClass` wins;
`"-"` forces `storageClassName: ""` for statically bound PVs). All claims —
data, recordings, media, music, postgres, loki, prometheus, grafana — are
created automatically. The exceptions are the library shares: give
`persistence.media` (or `.music`) an `existingClaim`, `hostPath` or `nfs`
and no PVC is created — the share is mounted directly.

```yaml
global: { storageClass: fast-nvme }
persistence:
  media: { hostPath: /tank/media }        # node path, no PV/PVC at all
  music: { enabled: true, nfs: { server: nas, path: /music } }
  recordings: { size: 2Ti, storageClass: bulk-hdd }   # overrides global
```

### Pre-provisioned volumes (static PVs, no dynamic provisioner)

When the data must live on a specific disk — or the cluster has no
provisioner — create the PersistentVolumes **before** `helm install` and let
the chart's claims bind to them. Two different mechanics are involved:

- **Ontele's claims** (`<release>-data`, `<release>-recordings`, and the
  auto-created `<release>-media`/`-music`) are created by the chart; give
  them `storageClass: "-"` so they render `storageClassName: ""` and bind
  statically. (Alternatively pre-create the PVCs yourself and pass
  `persistence.*.existingClaim`.)
- **PostgreSQL** is a StatefulSet: its claim comes from a
  `volumeClaimTemplate` and is always named `data-<release>-postgresql-0`
  — it cannot use `existingClaim`. Pin a PV to that exact name with
  `claimRef`.

[examples/static-volumes.yaml](examples/static-volumes.yaml) is a ready
manifest (edit namespace, release, node name, paths, sizes) and
[examples/static-volumes-values.yaml](examples/static-volumes-values.yaml)
the matching values:

```bash
# 1. directories on the node (local volumes do not create them)
ssh <node> sudo mkdir -p /srv/ontele/{postgres,data,recordings}

# 2. namespace + PVs first — claimRef pins each PV to one exact claim
#    (the example uses namespace "ontele"; claimRef.namespace MUST match -n below)
kubectl create namespace ontele
kubectl apply -f deploy/helm/ontele/examples/static-volumes.yaml

# 3. install; the "-" storage classes make the claims bind to those PVs
helm install ontele deploy/helm/ontele -n ontele \
  -f deploy/helm/ontele/examples/static-volumes-values.yaml \
  --set ingress.host=ontele.example.com

# 4. every claim should be Bound to YOUR pv, not a provisioned one
kubectl -n ontele get pvc
```

Rules that matter: keep `storageClass: "-"` in the values exactly as-is (it
renders `storageClassName: ""` — putting a real class name there routes the
claim to that provisioner instead); PV `capacity` ≥ the claim's `size`; `accessModes` must
match (`ReadWriteOnce`); keep `persistentVolumeReclaimPolicy: Retain` so
deleting the release never deletes the database or recordings; `local:` PVs
need the `nodeAffinity` block and pin the pod to that node. Loki, Prometheus
and Grafana keep the cluster's default StorageClass — on a cluster with no
provisioner at all, give them PVs + `"-"` the same way (or disable them).

### Troubleshooting Pending claims

`kubectl -n <ns> describe pvc <claim>` — the events name the culprit:

- **`ExternalProvisioning: waiting for ... provisioner`** — the claim has a
  real StorageClass, so it is asking a provisioner, not your static PVs.
  A `"-"` was replaced by a class name, or the values file wasn't passed on
  this `helm upgrade` (always include it — values resets re-class the claims).
  PVC `storageClassName` is immutable: delete the PVC (scale the workload
  down first) and re-upgrade with the right values.
- **PV shows `Released`** (`kubectl get pv`) — a Retain PV keeps the deleted
  claim's UID in `spec.claimRef.uid` after an uninstall/namespace delete and
  can never re-bind. Clear it and it becomes `Available`:
  `kubectl patch pv <pv> --type merge -p '{"spec":{"claimRef":{"uid":null,"resourceVersion":null}}}'`
- **`claimRef` namespace ≠ release namespace** — a PV reserved for
  `media/ontele-data` will never bind a claim in `ontele/`. Patch
  `spec.claimRef.namespace` (and clear the uid as above), or re-apply the PV
  manifest with the right namespace.
- **Password auth failures after re-binding** — the PV kept the old PGDATA,
  initialized under the *previous* install's generated Secret (kept Secrets
  die with their namespace). Either reset it to the new Secret's value:
  `ALTER USER ontele WITH PASSWORD '<current>'` via
  `kubectl exec ... psql`, or wipe the PV's postgres directory to re-initdb
  fresh.

## DVR post-processing (HandBrake)

The ontele image ships `HandBrakeCLI` and
[`handbrake-postprocess.sh`](../../../tools/handbrake-postprocess.sh). Set

```yaml
config:
  dvrPostCmd: /usr/local/bin/handbrake-postprocess.sh
persistence:
  media:
    readOnly: false   # the script files encodes INTO the share (default is ro)
```

(or the same field in Settings → Live TV & DVR) and every finished recording
is encoded (x264 mkv by default — `HB_OPTS`/`FF_OPTS`/`ENCODER` env override),
classified by its `SxxEyy` pattern, moved into `/media/tv/<Show>/Season NN/`
or `/media/movies/`, the original `.ts` removed, and the library rescanned.
One encode runs at a time; progress lands in the activity feed as
`dvr.postcmd`. `handbrake.enabled=true` additionally deploys the
jlesage/handbrake GUI container for manual work — give it `watch`/`output`
hostPaths or claims (its automated watch-folder is independent of the DVR
hook).

## Guide data straight from the tuner

No XMLTV subscription? [`tools/hdhr-xmltv.py`](../../../tools/hdhr-xmltv.py)
(stdlib-only Python) builds an XMLTV file from the HDHomeRun's own guide API:

```bash
# cron on any LAN host — writing into the media share makes it visible in-pod
17 */4 * * * hdhr-xmltv.py --device 192.168.1.27 --hours 24 --output /media/video/guide.xml
```

then set the guide source to `/media/guide.xml` (Settings → Live TV & DVR,
or `config.xmltv`). Channel numbers in the XMLTV match the tuner lineup, so
mapping is automatic.

## Using an existing observability stack

Run only what you're missing — each piece integrates with what you have:

```yaml
loki: { enabled: false }
promtail:
  enabled: true                            # or false if Alloy/Promtail already tails pods
  lokiUrl: http://loki.monitoring:3100/loki/api/v1/push
prometheus: { enabled: false }
metrics:
  serviceMonitor: { enabled: true }        # prometheus-operator scrapes /metrics
grafana:
  enabled: false
  sidecar: { enabled: true, annotations: { grafana_folder: Ontele } }
```

- Ontele logs JSON to stdout — any collector (Promtail, Alloy, Vector,
  Fluent Bit) picks it up; the useful labels are `level` and `target`
  (`ontele.http`, `ontele.activity`).
- `grafana.sidecar` publishes the dashboard (and datasources for any
  in-chart Loki/Prometheus) as ConfigMaps labelled `grafana_dashboard` /
  `grafana_datasource`, which the kube-prometheus-stack / grafana chart
  sidecar auto-imports. The dashboard binds datasource UIDs `loki` and
  `prometheus`.
- Pods also carry `prometheus.io/scrape` annotations for annotation-based
  scrapers.

## Exposing without a LoadBalancer (NodePort)

No ingress controller, no Gateway, no MetalLB? Put the front door on a
NodePort — the *proxy's* Service, not Ontele's:

```yaml
ingress: { enabled: false }
externalUrl: http://192.168.1.50:30080   # the node address you'll browse to
oauth2Proxy:
  service: { type: NodePort, nodePort: 30080 }
```

Then browse `http://<that-node-ip>:30080`. `externalUrl` matters: with no
ingress/gateway to derive it from, it is what the OAuth redirect, Dex issuer
and Grafana root URL are built on — without it, logins bounce to localhost. (`service.type`/`service.nodePort`
exist too, but that is Ontele itself — it bypasses identity and the
NetworkPolicy blocks it while the proxy is on; only for `config.auth: none`
LANs.)

## Gateway API (Cilium)

The chart speaks Gateway API natively. Prerequisites once per cluster: the
[Gateway API CRDs](https://gateway-api.sigs.k8s.io/) and Cilium installed
with `gatewayAPI.enabled=true`.

Bring-your-own Gateway — attach the HTTPRoute to it:

```yaml
ingress: { enabled: false }
httpRoute:
  enabled: true
  host: ontele.example.com
  tls: true                          # your listener terminates TLS
  parentRefs:
    - name: shared-gateway
      namespace: infra               # that listener must allow cross-namespace routes
```

Or let the chart run its own Cilium Gateway (`httpRoute.enabled` is implied
and `parentRefs` must stay empty — the route attaches to this Gateway):

```yaml
ingress: { enabled: false }
httpRoute: { host: ontele.example.com, tls: true }
gateway:
  enabled: true                      # gatewayClassName: cilium
  address: 192.168.1.240             # optional — pinned via Cilium LB-IPAM
  tls: { secretName: ontele-edge-tls }   # cert-manager or kubectl create secret tls
```

That renders a `Gateway` with http (redirect-to-https) + https listeners for
the host, the HTTPRoute (incl. `/dex` when Dex is on), and Cilium creates the
LoadBalancer Service for it. With `tls: false` it's a single plain-http
listener and no cert is needed. The NetworkPolicy stays correct under Cilium:
it only guards Ontele itself (proxy + Prometheus may connect); the proxy is
reachable from the Gateway.

## How identity flows

```
browser ─► Ingress/HTTPRoute ─► oauth2-proxy ─► ontele        (X-Forwarded-Email/User/Groups)
                     │                  └─► grafana /grafana/  (Grafana auth-proxy, auto sign-up)
                     └─► /dex ─► dex   (trial only)
```

Ontele never sees credentials; the NetworkPolicy guarantees the forwarded
headers can only originate from the proxy.

## Upgrading

**Chart versions restarted at 0.1.0 (2026-08)** — the pre-restart charts
(≤0.3.1) ran ontele/dex as Deployments; they are StatefulSets now (stable
pod names). Upgrading a pre-restart release: helm creates the StatefulSet
before removing the old Deployment, which would briefly run two writers
against the same RWO volumes — delete the old Deployments first:

```bash
kubectl -n <ns> delete deployment <release> <release>-dex --ignore-not-found --wait
helm upgrade ...
```


`helm upgrade` is safe: Ontele migrates its own schema at boot, settings
tuned in the UI are stored in Postgres and are not overwritten by chart
values, and generated secrets are re-read from the cluster.
