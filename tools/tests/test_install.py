"""Installer regressions; no GPU, privilege escalation or real user files."""
import mmap
import os
import shlex
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "install-driver.sh"
UNINSTALL = SCRIPT.with_name("uninstall-driver.sh")


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.dest = self.root / "driver dir"
        self.source = self.root / "build.so"
        self.source.write_bytes(b"old library contents")
        self.env = dict(os.environ, HOME=str(self.root), XDG_CONFIG_HOME=str(self.root / "config"))
        self.config = self.root / "config/environment.d/90-nvenc-vaapi.conf"

    def run_install(self, *args, success=True):
        result = subprocess.run(
            ["bash", str(SCRIPT), "--library", str(self.source), "--dest-dir", str(self.dest), *args],
            env=self.env, capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(result.returncode == 0, success, result.stderr)
        return result

    def test_update_preserves_mapped_library_and_replaces_destination_symlink(self):
        self.run_install()
        dest = self.dest / "nvidia_nvenc_drv_video.so"
        with dest.open("rb") as opened, mmap.mmap(opened.fileno(), 0, access=mmap.ACCESS_READ) as loaded:
            self.source.write_bytes(b"new library")
            self.run_install()
            self.assertEqual(loaded[:], b"old library contents")
            self.assertEqual(dest.read_bytes(), b"new library")
        unrelated = self.root / "unrelated"
        unrelated.write_bytes(b"untouched")
        dest.unlink()
        dest.symlink_to(unrelated)
        self.run_install()
        self.assertFalse(dest.is_symlink())
        self.assertEqual(unrelated.read_bytes(), b"untouched")

    def test_session_opt_in_and_removal_are_scoped(self):
        self.run_install()
        self.assertFalse(self.config.exists())
        self.run_install("--enable-session")
        content = self.config.read_text()
        self.assertIn('LIBVA_DRIVER_NAME=nvidia_nvenc\n', content)
        self.assertIn(f'LIBVA_DRIVERS_PATH="{self.dest}"\n', content)
        other = self.config.parent / "other.conf"
        other.write_text("OTHER=value\n")
        self.source.unlink()
        self.run_install("--disable-session")
        self.assertFalse(self.config.exists())
        self.assertTrue(other.exists())

    def test_session_file_is_understood_by_systemd_environment_generator(self):
        generator = Path("/usr/lib/systemd/user-environment-generators/30-systemd-environment-d-generator")
        if not generator.exists():
            self.skipTest("systemd environment generator unavailable")
        self.run_install("--enable-session")
        result = subprocess.run([str(generator)], env=self.env,
                                capture_output=True, text=True, timeout=10, check=True)
        assignments = {}
        for line in result.stdout.splitlines():
            if line.startswith("LIBVA_"):
                assignment = shlex.split(line)
                self.assertEqual(len(assignment), 1)
                key, value = assignment[0].split("=", 1)
                assignments[key] = value
        self.assertEqual(assignments["LIBVA_DRIVER_NAME"], "nvidia_nvenc")
        self.assertEqual(assignments["LIBVA_DRIVERS_PATH"], str(self.dest))

    def test_missing_library_does_not_damage_existing_installation(self):
        self.run_install()
        self.source.unlink()
        self.run_install(success=False)
        self.assertEqual((self.dest / "nvidia_nvenc_drv_video.so").read_bytes(), b"old library contents")
        self.assertEqual(list(self.dest.glob(".nvidia_nvenc.*")), [])

    def test_unmanaged_environment_file_is_never_overwritten_or_removed(self):
        self.config.parent.mkdir(parents=True)
        self.config.write_text("KEEP=this\n")
        for flag in ("--enable-session", "--disable-session"):
            self.run_install(flag, success=False)
            self.assertEqual(self.config.read_text(), "KEEP=this\n")
        self.assertFalse(self.dest.exists())

    def test_invalid_options_and_session_paths_leave_no_installation(self):
        self.run_install("--enable-session", "--disable-session", success=False)
        self.run_install("--dest-dir", success=False)
        self.run_install("--dest-dir", str(self.root / "$BAD"), "--enable-session", success=False)
        self.assertFalse(self.dest.exists())
        self.assertFalse(self.config.exists())

    def run_uninstall(self):
        fake_bin = self.root / "bin"
        fake_bin.mkdir(exist_ok=True)
        systemctl = fake_bin / "systemctl"
        if not systemctl.exists():
            systemctl.write_text("#!/bin/sh\nexit 1\n")
            systemctl.chmod(0o755)
        env = dict(self.env, PATH=str(fake_bin) + os.pathsep + os.environ["PATH"])
        env.pop("DISPLAY", None)
        env.pop("WAYLAND_DISPLAY", None)
        return subprocess.run(["bash", str(UNINSTALL), "--dest-dir", str(self.dest)],
                              env=env, cwd="/", capture_output=True, text=True, timeout=10, check=True)

    def test_tty_uninstall_removes_only_owned_files_and_is_repeatable(self):
        self.run_install("--enable-session")
        other = self.dest / "nvidia_drv_video.so"
        other.write_bytes(b"decoder")
        desktop = self.root / ".local/share/applications/vesktop-nvenc-vaapi.desktop"
        desktop.parent.mkdir(parents=True)
        desktop.write_text("# Managed by nvenc-vaapi-driver\n[Desktop Entry]\n")
        for _ in range(2):
            self.run_uninstall()
        self.assertFalse((self.dest / "nvidia_nvenc_drv_video.so").exists())
        self.assertFalse(self.config.exists())
        self.assertFalse(desktop.exists())
        self.assertEqual(other.read_bytes(), b"decoder")

    def test_uninstall_preserves_unmanaged_config_and_symlink_targets(self):
        self.run_install("--enable-session")
        self.config.write_text("KEEP=custom\n")
        desktop = self.root / ".local/share/applications/vesktop-nvenc-vaapi.desktop"
        desktop.parent.mkdir(parents=True)
        desktop.write_text("KEEP=launcher\n")
        victim = self.root / "other-library"
        victim.write_bytes(b"preserve")
        installed = self.dest / "nvidia_nvenc_drv_video.so"
        installed.unlink()
        installed.symlink_to(victim)
        self.run_uninstall()
        self.assertFalse(installed.is_symlink())
        self.assertEqual(victim.read_bytes(), b"preserve")
        self.assertEqual(self.config.read_text(), "KEEP=custom\n")
        self.assertEqual(desktop.read_text(), "KEEP=launcher\n")

    def test_uninstall_clears_user_manager_override_only_when_our_backend_is_selected(self):
        self.run_install()
        fake_bin = self.root / "bin"
        fake_bin.mkdir()
        systemctl = fake_bin / "systemctl"
        log = self.root / "systemctl.log"
        for backend in ("nvidia_nvenc", "nvidia"):
            systemctl.write_text("#!/bin/sh\n"
                'if [ "$2" = show-environment ]; then\n'
                f'  echo LIBVA_DRIVER_NAME={backend}\n'
                'elif [ "$2" = unset-environment ]; then\n'
                f'  echo "$*" >> "{log}"\n'
                'fi\n')
            systemctl.chmod(0o755)
            self.run_uninstall()
            calls = log.read_text().splitlines()
            self.assertEqual(len(calls), 1)
            self.assertEqual(calls[0], "--user unset-environment LIBVA_DRIVER_NAME LIBVA_DRIVERS_PATH NVENC_VAAPI_DEVICE")


if __name__ == "__main__":
    unittest.main()
