# A local Snowflake emulator (fakesnow, backed by DuckDB) that speaks the
# Snowflake REST protocol, for the Snowflake adapter's tests.
FROM python:3.12-slim
RUN pip install --no-cache-dir "fakesnow[server]==0.11.22"
EXPOSE 8000
CMD ["fakesnow", "-s", "--host", "0.0.0.0", "-p", "8000"]
