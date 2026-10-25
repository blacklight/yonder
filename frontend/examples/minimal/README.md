# minimal example frontend

The smallest valid alternative frontend: an `index.html` at the
directory root plus a `frontend.json` manifest.

Install it via the admin API:

```sh
cd examples/minimal
zip -r /tmp/minimal.zip .
curl -X POST http://localhost:3000/api/v1/yonder/admin/frontends \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -F name=minimal -F "file=@/tmp/minimal.zip"
# then PUT {"value": "minimal"} to /api/v1/yonder/admin/frontend
```

It also serves as the fixture shape the backend integration tests
exercise (root `index.html`, manifest, relative assets).
