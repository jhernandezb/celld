# Hyperdrive

A Hyperdrive binding gives a Worker the connection details of a PostgreSQL
database. celld connects the Worker directly to a database that the operator
names at deploy time. celld has no pooler and no query cache, so the binding
is the Hyperdrive API without the Hyperdrive service. Read the
[Cloudflare Hyperdrive documentation](https://developers.cloudflare.com/hyperdrive/)
for the API.

| | celld |
| --- | --- |
| Configuration | The Wrangler file deploys unchanged. A production deploy also needs one environment variable for each binding. |
| API | `env.HYPERDRIVE` has the properties and the `connect()` method of the Cloudflare binding, with the differences below. |
| Operation | Each request opens its own connection to the database. There is no pooler, and the driver negotiates TLS. |

## Example

The [Hyperdrive example](../../examples/hyperdrive) queries a database with
`pg`. Run `npm install` in its directory first.

<!-- celld-example: hyperdrive -->

## Connection string

A `hyperdrive` entry gives a `binding`, an `id`, and optionally a
`localConnectionString`. celld keeps the `id` because Wrangler requires it,
but celld does not look it up.

`celld deploy` reads the connection string of each binding from
`CELLD_HYPERDRIVE_CONNECTION_STRING_<BINDING>`, and it stops when the variable
is not set:

```sh
CELLD_HYPERDRIVE_CONNECTION_STRING_HYPERDRIVE='postgres://app:…@db.internal:5432/app' \
  celld deploy --bucket s3://fleet
```

`celld deploy` ignores `localConnectionString` and
`CLOUDFLARE_HYPERDRIVE_LOCAL_CONNECTION_STRING_<BINDING>`, as
`wrangler deploy` does. `celld dev` follows the rules of `wrangler dev`: it
uses `CLOUDFLARE_HYPERDRIVE_LOCAL_CONNECTION_STRING_<BINDING>` when it is set,
and `localConnectionString` otherwise.

The driver parses the connection string again, so celld accepts only strings
that the driver reads the same way as the binding's fields. A string outside
these rules stops the deployment, and the error names the part to fix:

- The scheme is `postgres://` or `postgresql://`. MySQL is not supported.
- The string contains only printable ASCII characters, without spaces or `#`.
  Percent-encode any other character. Every `%` starts a two-digit escape, so
  write a literal `%` as `%25`.
- The user is not empty. The password can be empty.
- The host is a name of ASCII letters, digits, `-`, `.`, and `_`, an IPv4
  address, or an IPv6 address in brackets. Write an IPv6 address in its
  shortest lowercase form, such as `[::1]` rather than `[0:0:0:0:0:0:0:1]`.
  The host has no escapes, so write an international name in its ASCII form.
- The port is from 1 to 65535. Without a port, celld uses 5432.
- The database is one path segment and is not `.` or `..`. A database name
  cannot contain the escapes `%23`, `%24`, `%26`, `%2B`, `%2C`, `%2F`, `%3A`,
  `%3B`, `%3D`, `%3F`, or `%40`, because drivers decode them differently.
- The query uses only these keys, each at most once: `sslmode`,
  `application_name`, `fallback_application_name`, `options`,
  `connect_timeout`, `statement_timeout`, `lock_timeout`,
  `idle_in_transaction_session_timeout`, and `query_timeout`. Keys that
  change the endpoint, the user, or TLS, such as `host`, `ssl`, or `sslcert`,
  stop the deployment.
- `sslmode` is `disable`, `require`, `verify-ca`, or `verify-full`. Use
  `require` or `disable` instead of `allow`, `prefer`, or `no-verify`.

The deploy report shows each binding as `postgres://host:port/database`,
without the user or the password.

## TLS

celld verifies every TLS connection: it checks the certificate chain and the
host name, for all of `require`, `verify-ca`, and `verify-full`. celld trusts
only the Mozilla root certificates, so TLS works only with a database whose
certificate chains to a public certificate authority. A certificate from a
private certificate authority, or a self-signed certificate, fails with
`UnknownIssuer` or `CaUsedAsEndEntity`. For such a database on a private
network, use `sslmode=disable`.

## API

The binding has the properties `connectionString`, `host`, `port`, `user`,
`password`, `database`, and `ip`, and the method `connect()`. A Worker can
connect in three ways:

- **`new Client({ connectionString })`** with `pg`. The driver reads `sslmode`
  from the string and negotiates TLS itself. This is the supported way.
- **`new Client({ host, port, user, password, database })`** with `pg`. The
  fields carry no `sslmode`, so pass `ssl: true` when the database requires
  TLS.
- **`env.HYPERDRIVE.connect()`** returns a
  [`cloudflare:sockets`](../cloudflare-compat.md) socket to the database,
  opened with `secureTransport: "starttls"`. The socket is plaintext until the
  caller calls `startTls()`. When the connection string requires TLS,
  `connect()` throws a `TypeError`, because celld does not negotiate TLS for
  the caller.

`pg` 8.23.1 with `pg-cloudflare` 1.4.1 and `pg-connection-string` 2.14.1 is
the tested driver, and the example pins these versions.

## Connections

celld closes the sockets of a request when the request ends, as workerd does.
A pool in module scope therefore cannot keep a connection for the next
request. Create a client for each request, and call `end()` in
`ctx.waitUntil()`.

Call `connect()` before the `try` block whose `finally` calls `end()`, as the
example does. When a TLS upgrade fails, `pg`'s `end()` never settles.

Each request pays for a TCP connection, a TLS handshake when TLS is required,
and a SCRAM authentication. The database needs about one connection for each
concurrent request. An external pooler such as PgBouncer in front of the
database reduces that cost. In PgBouncer's transaction mode, session state
such as `SET`, advisory locks, and `LISTEN` does not last past a transaction,
and named prepared statements (`client.query({ name })`) need PgBouncer 1.21
or later with `max_prepared_statements` set. Otherwise, use unnamed queries.

A `pg` connection string with an IPv6 address fails: `pg-cloudflare` rebuilds
the address without brackets, and the socket parser refuses it. Use a host
name, or connect with the fields, whose `host` keeps the brackets.

## Credentials

celld stores the connection string, with its password, in the deployment
manifest in the fleet bucket. The string is part of the deployment version, so
a new password needs a new deployment. Every earlier deployment keeps the
password it was deployed with, and the nodes can run two versions for a short
time during a rollout. To change a password:

1. Create the new credential while the old one still works.
2. Deploy with the new connection string.
3. Wait for every node to log `deployment_adopted` with the new version.
   Then revoke the old credential, unless you might still roll back to an
   earlier deployment that uses it.

## Transactions

celld holds a Durable Object's external effects until its own storage is
durable, but SQL in PostgreSQL does not commit together with Durable Object
storage. Make database writes idempotent: store an operation ID in the
Durable Object first, and enforce it with a unique constraint in PostgreSQL.
celld never retries a SQL statement.

## Differences from Cloudflare

- celld connects to the database directly. There is no pooler near the
  database and no query cache.
- `host`, `port`, and `connectionString` describe the database, not a
  Hyperdrive pooler.
- `ip` is `null`.
- The production connection string comes from
  `CELLD_HYPERDRIVE_CONNECTION_STRING_<BINDING>`, not from the `id`.
- TLS works only with a publicly trusted certificate, and `connect()` throws
  when the connection string requires TLS.
- A Worker Loader cannot pass the binding to a loaded Worker, which fails with
  `DataCloneError`. Pass `env.HYPERDRIVE.connectionString` instead.
- MySQL is not supported.

The [Cloudflare compatibility](../cloudflare-compat.md#services) page lists the
runtime APIs and the unsupported services.
