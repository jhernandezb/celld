// The Worker that `tests/hyperdrive_runtime.rs` drives. Each route checks one
// property of the Hyperdrive binding and answers with JSON.
import { DurableObject } from "cloudflare:workers";
import pg from "pg";
import pgConnectionString from "pg-connection-string";

// Module scope: a second request that sees the same token ran in the same
// isolate.
const token = crypto.randomUUID();
let served = 0;

const MEMBERS = ["connectionString", "host", "port", "user", "password", "database", "ip", "connect"];

export class Echo extends DurableObject {
  echo(value) {
    return value;
  }
}

function fields(binding) {
  const { connectionString, host, port, user, password, database } = binding;
  return { connectionString, host, port, user, password, database };
}

function leaks(binding, text) {
  return text.includes(binding.password) || text.includes(binding.connectionString);
}

async function caught(run) {
  try {
    return { ok: await run() };
  } catch (error) {
    return { error: error?.name, code: error?.code, message: String(error?.message ?? error) };
  }
}

function deadline(promise, ms, what) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`${what} took longer than ${ms} ms`)), ms);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

// A loaded Worker that answers with its own env, or with the outcome of a
// socket to `TARGET`.
const CHILD = `
import { connect } from "cloudflare:sockets";
export default {
  async fetch(request, env) {
    if (!env.TARGET) return new Response(JSON.stringify(env));
    try {
      const socket = connect(env.TARGET);
      await socket.opened;
      await socket.close();
      return new Response("opened");
    } catch (error) {
      return new Response("refused: " + error.name + ": " + error.message);
    }
  },
};`;

async function child(env, childEnv, globalOutbound) {
  const code = {
    compatibilityDate: "2026-01-01",
    mainModule: "child.js",
    modules: { "child.js": CHILD },
    env: childEnv,
  };
  if (globalOutbound !== undefined) code.globalOutbound = globalOutbound;
  const response = await env.LOADER.load(code).getEntrypoint().fetch("http://child/");
  return await response.text();
}

class Wrapper {
  constructor(binding) {
    this.binding = binding;
  }
}

// A `pg` client from the connection string or from the fields. connect()
// stays outside the try: after a failed TLS upgrade, end() never settles.
async function query(ctx, config, sql) {
  const client = new pg.Client({ ...config, connectionTimeoutMillis: 5000 });
  await deadline(client.connect(), 10000, "connect");
  try {
    return (await deadline(client.query(sql), 10000, "query")).rows;
  } finally {
    ctx.waitUntil(client.end());
  }
}

function clientConfig(binding, mode) {
  if (mode === "url") return { connectionString: binding.connectionString };
  const config = {
    host: binding.host,
    port: binding.port,
    user: binding.user,
    password: binding.password,
    database: binding.database,
  };
  return config;
}

const IDENTITY = "select current_user as user, current_database() as database";

const routes = {
  async "/token"() {
    served += 1;
    return { token, served };
  },

  async "/shape"(_request, env) {
    const binding = env.HYPERDRIVE;
    let assigned;
    try {
      binding.host = "elsewhere";
      assigned = "no error";
    } catch (error) {
      assigned = error.name;
    }
    return {
      ownKeys: Reflect.ownKeys(binding).map(String),
      frozen: Object.isFrozen(binding),
      members: Object.fromEntries(MEMBERS.map((name) => [name, typeof binding[name]])),
      ip: binding.ip,
      hostAfterAssignment: binding.host,
      assigned,
      fields: Object.fromEntries(
        ["HYPERDRIVE", "HD_TLS", "HD_CAFE", "HD_V6"].map((name) => [name, fields(env[name])]),
      ),
    };
  },

  async "/copies"(_request, env) {
    const binding = env.HYPERDRIVE;
    const copies = {
      structuredClone: (await caught(() => JSON.stringify(structuredClone(binding)))).ok ?? "threw",
      spread: JSON.stringify({ ...binding }),
      json: JSON.stringify(binding),
      assign: JSON.stringify(Object.assign({}, binding)),
      entries: JSON.stringify(Object.entries(binding)),
    };
    return { copies, leaks: Object.values(copies).some((copy) => leaks(binding, copy)) };
  },

  async "/loader"(_request, env) {
    const binding = env.HYPERDRIVE;
    const attempt = (childEnv) => caught(() => child(env, childEnv));
    const outcomes = {
      direct: await attempt({ DB: binding }),
      nested: await attempt({ CFG: { db: binding } }),
      array: await attempt({ CFG: [binding] }),
      wrapped: await attempt({ CFG: new Wrapper(binding) }),
      string: await attempt({ URL: binding.connectionString }),
    };
    return {
      outcomes,
      wrappedLeaks: leaks(binding, JSON.stringify(outcomes.wrapped)),
      stringArrived: outcomes.string.ok?.includes(JSON.stringify(binding.connectionString)) ?? false,
    };
  },

  // A loaded Worker given the connection string cannot use it when its
  // egress is denied. The target is this server's own listener, which accepts.
  async "/egress"(request, env) {
    const target = new URL(request.url).host;
    const childEnv = { URL: env.HYPERDRIVE.connectionString, TARGET: target };
    return {
      allowed: await child(env, childEnv),
      denied: await child(env, childEnv, null),
    };
  },

  async "/rpc"(_request, env) {
    const binding = env.HYPERDRIVE;
    const stub = env.ECHO.get(env.ECHO.idFromName("echo"));
    const outcome = await caught(async () => JSON.stringify((await stub.echo(binding)) ?? null));
    return { outcome, leaks: leaks(binding, JSON.stringify(outcome)) };
  },

  async "/connect"(_request, env) {
    const required = await caught(() => env.HD_TLS.connect());
    const socket = env.HYPERDRIVE.connect();
    socket.opened.catch(() => {});
    const plain = {
      startTls: typeof socket.startTls,
      opened: typeof socket.opened?.then,
    };
    socket.close().catch(() => {});
    return { required, plain };
  },

  async "/agree"(request) {
    const url = new URL(request.url).searchParams.get("url");
    const parse = pgConnectionString.parse ?? pgConnectionString;
    const { host, port, user, password, database } = parse(url);
    return { host, port, user, password, database };
  },

  // PostgreSQL routes; the test runs them only against a database.

  async "/pg"(request, env, ctx) {
    const params = new URL(request.url).searchParams;
    const binding = env[params.get("binding")];
    return (await query(ctx, clientConfig(binding, params.get("mode")), IDENTITY))[0];
  },

  // This is a disposable superuser fixture, never a production database.
  // Return booleans only: password verifiers must not leave PostgreSQL.
  async "/pg/auth-config"(_request, env, ctx) {
    return (await query(ctx, clientConfig(env.HYPERDRIVE, "url"), `
      select
        (select rolpassword like 'SCRAM-SHA-256$%'
         from pg_authid where rolname = current_user) as scram_password,
        (select count(*) > 0 and bool_and(auth_method = 'scram-sha-256')
         from pg_hba_file_rules where type like 'host%') as scram_hosts,
        (select count(*) = 0
         from pg_hba_file_rules where error is not null) as valid_hba
    `))[0];
  },

  async "/pg/wrong-password"(request, env, ctx) {
    const params = new URL(request.url).searchParams;
    const binding = env[params.get("binding")];
    const config = clientConfig(binding, params.get("mode"));
    const wrongPassword = binding.password + "-incorrect";
    if (config.connectionString) {
      const url = new URL(config.connectionString);
      url.password = wrongPassword;
      config.connectionString = url.href;
    } else {
      config.password = wrongPassword;
    }
    return await caught(() => query(ctx, config, IDENTITY));
  },

  async "/pg/setup"(_request, env, ctx) {
    const created = await caught(() =>
      query(ctx, clientConfig(env.HYPERDRIVE, "url"), 'create database "café"'),
    );
    // 42P04: it exists from an earlier run.
    if (created.error && !created.message.includes("already exists")) throw new Error(created.message);
    return { created: !created.error };
  },

  async "/pg/recover"(_request, env, ctx) {
    const failed = await caught(() =>
      query(ctx, { ...clientConfig(env.HYPERDRIVE, "fields"), port: 1 }, "select 1"),
    );
    const rows = await query(ctx, clientConfig(env.HYPERDRIVE, "url"), IDENTITY);
    return { failed, recovered: rows[0] };
  },

  // The plaintext socket reaches PostgreSQL: an SSLRequest gets its one-byte
  // answer, `S` or `N`.
  async "/pg/socket"(_request, env) {
    const socket = env.HYPERDRIVE.connect();
    try {
      await deadline(socket.opened, 10000, "open");
      const writer = socket.writable.getWriter();
      await writer.write(new Uint8Array([0, 0, 0, 8, 4, 210, 22, 47]));
      writer.releaseLock();
      const reader = socket.readable.getReader();
      const { value } = await deadline(reader.read(), 10000, "read");
      reader.releaseLock();
      return { answer: String.fromCharCode(value[0]) };
    } finally {
      await socket.close().catch(() => {});
    }
  },

  async "/pg/untrusted"(_request, env, ctx) {
    return await caught(() => query(ctx, clientConfig(env.HD_TLS, "url"), "select 1"));
  },
};

export default {
  async fetch(request, env, ctx) {
    const route = routes[new URL(request.url).pathname];
    if (!route) return new Response("no such route", { status: 404 });
    try {
      return Response.json(await route(request, env, ctx));
    } catch (error) {
      return Response.json(
        { error: error?.name, message: String(error?.message ?? error) },
        { status: 500 },
      );
    }
  },
};
