# Container account storage

Images create an empty `/app/backend/users` directory. Runtime accounts, password hashes, local credential references and private keys are excluded from the Docker build context; account files are never copied into image layers.

Existing Compose deployments retain the `./backend/users:/app/backend/users` bind mount, including their existing accounts. Pulling or rebuilding the image does not replace that mounted file. Keep the directory private and back it up independently of the image.

For a new installation, create the first application administrator locally before exposing the service:

```sh
python3 backend/tools/users_config_tui.py --file /private/station-users/users.json
```

Add the user using the password prompt, then grant its application `admin` role in the role editor. Mount that directory at `/app/backend/users` when starting a standalone container, or use the existing Compose accounts directory. Passwords should not be supplied as command-line arguments. The SSH/CLI and graphical account tools remain available in the source checkout.

The backend creates an empty account document if the mounted file is absent. This does not create a default administrator or grant anonymous control. A custom `GS_USERS_FILE` remains supported; mount its private persistent storage explicitly. Runtime accounts already present in the working tree are left untouched.
