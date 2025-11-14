mod c_system_test {
    use std::path::PathBuf;
    use std::process::Command;


    #[test]
    fn run_c_system_test() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("c-system-test");

        let status = Command::new("cmake")
            .arg("-S")
            .arg(".")
            .arg("-B")
            .arg("build")
            .arg("-DCMAKE_BUILD_TYPE=Debug")
            .current_dir(&root)
            .status()
            .expect("Failed to config cmake build");
        assert!(status.success(), "CMake config failed");

        // Build the C project
        let status = Command::new("cmake")
            .arg("--build")
            .arg("build")
            .current_dir(&root)
            .status()
            .expect("Failed to invoke cmake build");
        assert!(status.success(), "CMake build failed");

        // Path to the built executable
        let exe = root.join("build").join("c_system_test");

        // Run the test executable
        let output = Command::new(&exe)
            .current_dir(&root)
            .output()
            .expect("Failed to run c_system_test");

        // Print stdout/stderr for debugging
        eprintln!("stdout:\n{}", String::from_utf8_lossy(&output.stdout));
        eprintln!("stderr:\n{}", String::from_utf8_lossy(&output.stderr));

        // Assert exit success
        assert!(
            output.status.success(),
            "C system test failed with exit code {:?}",
            output.status.code()
        );
    }
}
