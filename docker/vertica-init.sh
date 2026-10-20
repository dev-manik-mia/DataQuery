#!/bin/bash
# The Vertica image ships the server but no database: generate the node
# management agent's certificates, start the agent, create a single-node
# database (Community Edition license), seed it, then stay in the foreground.
set -eu
export USER=dbadmin
data=/tmp/vertica
certs=/opt/vertica/config/https_certs
(
  cd "$certs"
  VTMPDIR=/tmp /opt/vertica/bin/gen_httpstls_json.sh > /dev/null 2>&1
  mv rootca_cert.pem rootca.pem
  mv dbadmin_cert.pem dbadmin.pem
  mv dbadmin_key.pem dbadmin.key
  mv nma_cert.pem vertica_https.pem
  mv nma_key.pem vertica_https.key
)
/opt/vertica/bin/manage_node_agent.sh start
for _ in $(seq 1 60); do
  (exec 3<>/dev/tcp/127.0.0.1/5554) 2>/dev/null && break
  sleep 1
done
/opt/vertica/bin/vcluster create_db --db-name "$VERTICA_DB" --hosts 127.0.0.1 \
  --catalog-path "$data" --data-path "$data" --password "$VERTICA_PASSWORD" \
  --force-removal-at-creation
/opt/vertica/bin/vsql -U dbadmin -w "$VERTICA_PASSWORD" -d "$VERTICA_DB" -v ON_ERROR_STOP=1 \
  -f /seed/vertica-seed.sql && touch /tmp/.seeded || echo "The seed failed."
exec tail -f /dev/null
