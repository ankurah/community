# Community GKE deployment

Community runs as one durable Ankurah server in the `community` namespace of
the shared `kube-1` Autopilot cluster. It shares the cluster, Cloud SQL
instance, and global load balancer with other incubator services, but has its
own database, login role, secrets, workload identities, namespace, pod,
Artifact Registry repository, backend service, and certificate.

The checked-in Deployment is fail-closed. Its image sentinel is never applied
directly; the deploy workflow replaces it with the immutable digest published
for the exact successful `main` commit.

Namespace and RBAC are bootstrap resources and are intentionally outside the
normal deploy identity's permissions:

```sh
kubectl apply -f infra/gke/namespace.yaml
kubectl apply -f infra/gke/deploy-rbac.yaml
```

Runtime secrets remain in Google Secret Manager in `synesthetic-1`. GKE Secret
Sync copies only the Community database URL, JWT signing key, CI hook key, and
APNs provider key + key identifier into the namespace-local
`community-runtime-secrets` Kubernetes Secret. The APNs team and app topic are
non-secret Deployment values. The application runtime identity has Cloud SQL
client access but no general Secret Manager access.

The Service creates standalone zonal NEGs named `community-web-gke-neg`. The
pod is pinned to `us-west1-b`; only that zone's non-empty NEG is attached to
`community-web-backend`. The shared URL map routes only
`community.ankurah.org` to that backend, leaving IDP hosts isolated.

The backend service overwrites `X-Forwarded-For` with the global load
balancer's trusted `{client_ip_address}` value. `server/src/guest.rs` therefore
continues to receive the single trusted address its guest-mint limiter expects,
rather than the default `<client-ip>,<load-balancer-ip>` pair.
