# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company

import unittest

import os
import tempfile

from spring_classpath import materialize, version_conflicts

H = "a" * 40
JAR = "/root/.gradle/caches/modules-2/files-2.1/%s/%s/%s/" + H + "/%s.jar"


class VersionConflicts(unittest.TestCase):
    def test_two_versions_of_one_artifact_are_reported(self):
        paths = [JAR % ("org.junit.jupiter", "junit-jupiter-engine", v, "x") for v in ("5.12.2", "6.1.2")]
        self.assertEqual(version_conflicts(paths),
                         {"org.junit.jupiter:junit-jupiter-engine": ["5.12.2", "6.1.2"]})

    def test_classifiers_and_project_dirs_are_not_conflicts(self):
        paths = [JAR % ("g", "a", "1", "a-1"), JAR % ("g", "a", "1", "a-1-tests"),
                 "/root/CVM/apps/spring-framework/spring-core/build/classes/java/test"]
        self.assertEqual(version_conflicts(paths), {})


class Materialize(unittest.TestCase):
    def test_unbuilt_project_jar_becomes_its_class_dirs(self):
        with tempfile.TemporaryDirectory() as d:
            os.makedirs(f"{d}/m/build/classes/java/main")
            os.makedirs(f"{d}/m/build/classes/java/testFixtures")
            self.assertEqual(materialize(f"{d}/m/build/libs/m-1.jar"), [f"{d}/m/build/classes/java/main"])
            self.assertEqual(materialize(f"{d}/m/build/libs/m-1-test-fixtures.jar"),
                             [f"{d}/m/build/classes/java/testFixtures"])

    def test_existing_and_foreign_entries_pass_through(self):
        self.assertEqual(materialize("/x/files-2.1/a.jar"), ["/x/files-2.1/a.jar"])


if __name__ == "__main__":
    unittest.main()
