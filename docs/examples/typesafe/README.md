# TypeSafe / Jev (HedronDB)

Bundled Facet OpenCollection for the **intent vs evidence** named ask.
Facet is the transport SoT; HedronDB does not link a TypeSafe SDK and does
not store `$TYPESAFE_API_KEY` in fixtures, vault notes, or SQLite.

Hydrate the secret in Facet, then call through `hedron jev-intent` (or MCP
`jev_intent`), which prefers `facet request run` against this file.

```bash
facet env set docs/examples/typesafe --environment typesafe \
  --name typesafeApiKey --value "$TYPESAFE_API_KEY" --secret
```

`jevShadow=true`. A Choice is not a write grant. See [docs/jev-native.md](../../jev-native.md).
