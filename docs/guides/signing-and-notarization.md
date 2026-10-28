# Signing and Notarization

`scripts/make-app.sh` signs Chronicle.app with a Developer ID certificate in
every build mode. The `dist` mode also builds a DMG and has Apple notarize it.
This guide covers the one-time setup on a Mac and what changes after the first
signed build.

Both the certificate and notarization need a paid Apple Developer Program
membership.

## Certificate

The script signs with a Developer ID Application certificate from your login
keychain.

1. Open Keychain Access. Choose Keychain Access > Certificate Assistant >
   Request a Certificate From a Certificate Authority.
2. Enter your email and name. Choose "Saved to disk" and save the
   `.certSigningRequest` file.
3. Sign in at <https://developer.apple.com/account/resources/certificates>.
   Click +, choose Developer ID Application, pick the G2 Sub-CA, and upload the
   request file.
4. Download the `.cer` file and double-click it. Keychain Access adds it to your
   login keychain, next to the private key from step 1.
5. Check that macOS sees it:

   ```sh
   security find-identity -v -p codesigning
   ```

   You should see one line with
   `"Developer ID Application: <your name> (<team ID>)"`.

The script finds that identity on its own. After a renewal you may have two
valid Developer ID Application certificates. The script then stops and lists
both. A renewed certificate usually keeps the old one's name, so pick one by
its SHA-1 hash, the 40-character value in the `find-identity` output:

```sh
export CHRONICLE_SIGN_IDENTITY=0123456789ABCDEF0123456789ABCDEF01234567
```

Copy the hash as printed, in uppercase. A name works too when it matches only
one certificate.

## Notary profile

`dist` sends the DMG to Apple's notary service. `notarytool` reads its
credentials from a keychain profile. No key or password goes in the repo or on
a command line the script runs.

1. In App Store Connect, open Users and Access > Integrations > App Store
   Connect API. Under Team Keys, generate a key with the Developer role.
2. Download the `.p8` file. Apple lets you download it only once. Note the key
   ID and the issuer ID shown on that page.
3. Store the profile:

   ```sh
   xcrun notarytool store-credentials chronicle-notary \
       --key ~/Downloads/AuthKey_<key ID>.p8 --key-id <key ID> --issuer <issuer ID>
   ```

4. Check that it works:

   ```sh
   xcrun notarytool history --keychain-profile chronicle-notary
   ```

5. Keep the `.p8` file somewhere safe, outside the repo.

The script uses the profile named `chronicle-notary`. To use another name:

```sh
export CHRONICLE_NOTARY_PROFILE=my-profile
```

## Modes

| Command | Build | Signature | Contacts Apple |
|---|---|---|---|
| `scripts/make-app.sh` or `scripts/make-app.sh debug` | debug | Developer ID, hardened runtime, `get-task-allow`, no secure timestamp | No |
| `scripts/make-app.sh release` | release | Developer ID, hardened runtime, secure timestamp | Timestamp server |
| `scripts/make-app.sh dist` | release | Same as `release`, plus a signed, notarized and stapled DMG | Timestamp server and notary service |

All three write `dist/Chronicle.app`. `dist` also writes
`dist/Chronicle-<version>.dmg`.

- `dist` refuses to run with uncommitted changes or untracked files. The build
  number is the commit count, so a dirty build would ship code that number
  doesn't name.
- Notarization usually takes a few minutes. The script waits up to 30 minutes.
- Debug builds carry `get-task-allow`, so a debugger can attach. Release and
  dist builds don't, and the script checks that.
- Every mode stops before it builds when it can't find exactly one usable
  certificate. There is no ad-hoc fallback, and the previous bundle stays.

## First launch

macOS ties Screen Recording and Microphone grants to an app's signature. Builds
before Developer ID signing were ad-hoc signed, and their grants don't carry
over. After your first Developer ID build, grant both once more. Later rebuilds
keep the grants, because the signature names the identifier and team instead of
the code hash.

The bundled daemon gets its own grants only when it runs as its own process,
for example under launchd. A daemon you start from Terminal uses Terminal's
grants. Nothing in the repo starts the bundled daemon as its own process yet.
That comes with login-item registration. Until then, these steps apply when you
run it under launchd yourself.

1. Open System Settings > Privacy & Security > Screen & System Audio Recording
   (called Screen Recording on macOS 14).
   Remove any old `chronicle-daemon` or `Chronicle` entry with the − button.
2. The daemon checks Screen Recording at startup and exits when it's missing.
   It doesn't show a prompt. So add it by hand: click +, press Cmd-Shift-G,
   paste the full path to `Chronicle.app/Contents/MacOS/chronicle-daemon`, and
   click Open. Turn the new entry on.
3. Start the daemon as its own process and turn the mic on from the menu bar. macOS asks whether
   `chronicle-daemon` may use the microphone. Click Allow.

## Troubleshooting

**The script found 0 identities, but you made a certificate.**
`security find-identity -v` lists only valid identities. Check that the
certificate hasn't expired and that its private key is in the same keychain.
Check that `CHRONICLE_SIGN_IDENTITY` is unset or names a current certificate. If
`security find-identity -p codesigning` without `-v` shows it as
`CSSMERR_TP_NOT_TRUSTED`, install the Developer ID G2 intermediate certificate
from <https://www.apple.com/certificateauthority/>.

**Notarization was rejected.** The script prints the submission ID and the
notary log. The log is JSON. Each entry under `issues` names a file in the DMG
and what is wrong with it. Common causes are a binary without the hardened
runtime, a signature without a secure timestamp, and unsigned nested code. Fix
the cause and run `dist` again. To fetch a log later:

```sh
xcrun notarytool log <submission ID> --keychain-profile chronicle-notary
```

**Notarization timed out.** Apple keeps processing after the script stops
waiting. The script prints the two commands that finish the job, a
`notarytool wait` and a `stapler staple`.

**Reading `spctl --assess`.** `dist` runs this on the stapled DMG:

```sh
spctl --assess --type open --context context:primary-signature -v dist/Chronicle-<version>.dmg
```

- `accepted` with `source=Notarized Developer ID` means Gatekeeper will open
  the DMG on another Mac.
- `rejected` with `source=Unnotarized Developer ID` means the signature is
  good but Apple has no notarization ticket for it.
- `rejected` with any other source means the signature is broken or isn't from
  a Developer ID certificate.
