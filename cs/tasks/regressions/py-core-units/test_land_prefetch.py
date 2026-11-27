import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "ops/bin"))
import _land_check


@unittest.skipUnless(sys.version_info >= (3, 11), "land dependency lockfiles require the runtime Python")
class LandPrefetchTests(unittest.TestCase):
    def setUp(self):
        directory = self.enterContext(tempfile.TemporaryDirectory())
        self.root = Path(directory).resolve()
        self.main = self.root / "main"
        self.lane = self.root / "lane"
        self.scratch = self.root / "scratch"
        self.scratch.mkdir()
        self.home = self.root / "home"
        self.enterContext(patch.dict(os.environ, {"HOME": str(self.home)}, clear=True))
        self.enterContext(patch.object(_land_check.shutil, "which", return_value="/trusted/tool"))
        self.flux = {"http-basic": {"composer.fluxui.dev": {"username": "flux-user", "password": "flux-secret"}}}

    def write_auth(self, path, contents, mode=0o600):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(contents))
        path.chmod(mode)
        return path

    def fetch_flux(self, run, record):
        project = self.lane / "clients/web"
        project.mkdir(parents=True, exist_ok=True)
        (project / "composer.json").write_text("{}")
        (project / "composer.lock").write_text(json.dumps({"packages": [{"dist": {
            "type": "zip", "url": "https://composer.fluxui.dev/download/abc123/flux-pro-1.2.zip"
        }}]}))
        return _land_check.composer_dependencies(self.main, self.lane, self.scratch,
                                                 {"PATH": os.defpath}, run, record)

    def test_prefetch_uses_only_flux_credentials_from_main_when_home_has_none(self):
        other = {"other.example": {"username": "other-user", "password": "other-secret"}}
        self.write_auth(self.home / ".composer/auth.json", {"http-basic": other})
        self.write_auth(self.main / "clients/web/auth.json",
                        {"http-basic": {**self.flux["http-basic"], **other}})
        self.write_auth(self.lane / "clients/web/auth.json", {"bearer": {"composer.fluxui.dev": "lane-secret"}})

        def run(argv, cwd, env):
            self.assertEqual(json.loads(env["COMPOSER_AUTH"]), self.flux)
            self.assertNotIn("flux-secret", str(argv))
            self.assertFalse((cwd / "auth.json").exists())
            (cwd / "vendor").mkdir()
            return 0, ""

        record = []
        self.assertTrue(self.fetch_flux(run, record))
        self.assertEqual(record, [{"command": "composer install", "status": 0}])

    def test_missing_credentials_do_not_use_lane_auth(self):
        self.write_auth(self.lane / "clients/web/auth.json", self.flux)
        with self.assertRaisesRegex(RuntimeError, "no credentials for composer.fluxui.dev: put them in"
                                    " ~/.composer/auth.json or .*main/clients/web/auth.json"):
            self.fetch_flux(lambda *_: self.fail("Composer ran without Flux credentials"), [])

    def test_refuses_readable_or_symlinked_auth_file(self):
        path = self.write_auth(self.main / "clients/web/auth.json", self.flux, mode=0o644)
        with self.assertRaisesRegex(RuntimeError, "unsafe Composer credentials file"):
            _land_check.composer_auth(self.main)
        path.rename(path.with_name("target.json")).chmod(0o600)
        path.symlink_to("target.json")
        with self.assertRaisesRegex(RuntimeError, "unsafe Composer credentials file"):
            _land_check.composer_auth(self.main)

    def test_composer_failure_surfaces_sanitized_last_error(self):
        self.write_auth(self.main / "clients/web/auth.json", self.flux)
        record = []
        with self.assertRaises(RuntimeError) as raised:
            self.fetch_flux(lambda *_: (100, "Earlier output\n"
                "Failed to download https://composer.fluxui.dev/download/abc123/flux-pro-1.2.zip (HTTP 403)\n"
                "\ninstall [--prefer-dist] [--no-dev]\n"), record)
        self.assertEqual(str(raised.exception), "composer install pre-fetch failed (100): "
                         "Failed to download https://composer.fluxui.dev/download/[redacted]/flux-pro-1.2.zip (HTTP 403)")
        self.assertEqual(record, [{"command": "composer install", "status": 100}])
        self.assertEqual(_land_check.composer_error("Rejected password flux-secret", self.flux),
                         "[redacted Composer error]")

    def test_new_locked_dependencies_are_prepared_before_running_offline(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            checkout, scratch, target, home = (root / name for name in ("wt", "scratch", "target", "home"))
            for path in (checkout, scratch, target, home):
                path.mkdir()
            registry = root / "registry"
            registry.mkdir()
            (registry / "cached").touch()
            toolchains = [{"name": "cargo", "bin": str(root), "rustup": str(root),
                           "toolchain": str(root), "registry": str(registry)}]

            def write(relative, contents):
                path = checkout / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(contents)

            write("Cargo.toml", '[dependencies]\nadded = "1"\n')
            write("Cargo.lock", '[[package]]\nname = "added"\nsource = '
                  '"registry+https://github.com/rust-lang/crates.io-index"\n')
            write("cs/pyproject.toml", '[project]\nname = "fixture"\ndependencies = ["added==1"]\n')
            write("cs/uv.lock", '[[package]]\nname = "added"\nsource = { registry = "https://pypi.org/simple" }\n')
            write("cs/.python-version", "3.13\n")
            for name in ("cs/tools/defuddle", "clients/web"):
                write(name + "/package.json", json.dumps({"dependencies": {"added": "npm:@fixture/added@1"},
                      "scripts": {"postinstall": "exit 99"}}))
                write(name + "/package-lock.json", json.dumps({"lockfileVersion": 3, "packages": {
                      "": {"dependencies": {"added": "npm:@fixture/added@1"}},
                      "node_modules/added": {"resolved": "https://registry.npmjs.org/added/-/added-1.tgz",
                                             "integrity": "sha512-fixture"}}}))
            write("clients/web/composer.json", json.dumps({"require": {"vendor/added": "1"},
                  "scripts": {"post-install-cmd": ["exit 99"]}}))
            write("clients/web/composer.lock", json.dumps({"packages": [{"name": "vendor/added",
                  "dist": {"type": "zip", "url": "https://api.github.com/repos/vendor/added/zipball/abc"}}]}))

            def run(argv, cwd, env):
                self.assertTrue(cwd.is_relative_to(scratch))
                if argv[0] == "cargo":
                    self.assertEqual(argv[1:3], ["fetch", "--locked"])
                    self.assertEqual(env["CARGO_NET_OFFLINE"], "false")
                    self.assertIn('name = "added"', (checkout / "Cargo.lock").read_text())
                    local_registry = Path(env["CARGO_HOME"]) / "registry"
                    self.assertTrue((local_registry / "cached").is_file())
                    (local_registry / "added").touch()
                if argv[0] == "uv":
                    self.assertIn("--no-build", argv)
                    self.assertEqual(env["UV_OFFLINE"], "0")
                    self.assertIn('name = "added"', (cwd / "uv.lock").read_text())
                    cache = Path(env["UV_CACHE_DIR"])
                    cache.mkdir()
                    (cache / "added.whl").touch()
                if argv[0] == "npm":
                    self.assertIn("--ignore-scripts", argv)
                    # npm refuses one file loaded as both user and global config ("double-loading config").
                    configs = {a.split("=", 1)[0]: a.split("=", 1)[1] for a in argv
                               if a.startswith(("--userconfig=", "--globalconfig="))}
                    self.assertEqual(set(configs), {"--userconfig", "--globalconfig"})
                    self.assertNotEqual(configs["--userconfig"], configs["--globalconfig"])
                    self.assertTrue(all(Path(p).is_file() and Path(p).read_text() == "" for p in configs.values()))
                    self.assertNotIn("scripts", json.loads((cwd / "package.json").read_text()))
                    (cwd / "node_modules/added").mkdir(parents=True)
                if argv[0] == "composer":
                    self.assertIn("--no-plugins", argv)
                    self.assertIn("--no-scripts", argv)
                    self.assertNotIn("scripts", json.loads((cwd / "composer.json").read_text()))
                    (cwd / "vendor/added").mkdir(parents=True)
                return 0, ""

            env, record = {"PATH": os.defpath, "HOME": str(home)}, []
            with patch.object(_land_check._toolchains, "land_entries", return_value=toolchains):
                _, web_build = _land_check.prepare_dependencies(
                    self.main, checkout, target, scratch, home, env, run, record)
            self.assertTrue(web_build)
            self.assertEqual([item["command"] for item in record],
                             ["cargo fetch", "uv sync", "npm ci", "composer install", "npm ci"])
            self.assertTrue(all(item["status"] == 0 for item in record))
            self.assertEqual(env["CARGO_NET_OFFLINE"], "true")
            self.assertEqual(env["UV_OFFLINE"], "1")
            self.assertEqual(env["npm_config_offline"], "true")
            self.assertEqual(env["COMPOSER_DISABLE_NETWORK"], "1")
            self.assertEqual((checkout / "target").resolve(), target)
            self.assertTrue((Path(env["CARGO_HOME"]) / "registry/added").is_file())
            self.assertFalse((registry / "added").exists())
            self.assertTrue((Path(env["UV_CACHE_DIR"]) / "added.whl").is_file())
            self.assertTrue((checkout / "cs/tools/defuddle/node_modules/added").is_dir())
            self.assertTrue((checkout / "clients/web/vendor/added").is_dir())
