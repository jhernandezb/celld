import { Client } from "pg";

export default {
  async fetch(request, env, ctx) {
    // One client for each request: celld closes a request's sockets when the
    // request ends, so a client cannot outlive it.
    const client = new Client({
      connectionString: env.HYPERDRIVE.connectionString,
      connectionTimeoutMillis: 5000,
    });
    // connect() stays outside the try. After a failed TLS upgrade, `pg`'s
    // end() never settles.
    await client.connect();
    try {
      const { rows } = await client.query(
        "select current_database() as database, now() as time",
      );
      return Response.json(rows[0]);
    } finally {
      ctx.waitUntil(client.end());
    }
  },
};
