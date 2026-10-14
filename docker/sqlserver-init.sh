#!/bin/bash
# Starts SQL Server, seeds it once, then keeps the server in the foreground.
set -e
/opt/mssql/bin/sqlservr &
server=$!
sqlcmd() { /opt/mssql-tools18/bin/sqlcmd -C -S localhost -U sa -P "$MSSQL_SA_PASSWORD" "$@"; }
for _ in $(seq 1 120); do
  sqlcmd -Q "SELECT 1" > /dev/null 2>&1 && break
  sleep 1
done
if [ ! -f /var/opt/mssql/.seeded ]; then
  sqlcmd -b -i /seed/sqlserver-seed.sql && touch /var/opt/mssql/.seeded
fi
wait $server
