use std::path::Path;

use scip_php::indexer::Indexer;
use scip_php::project::PhpProject;

#[test]
fn test_index_sample_project() {
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample-project");

    let project = PhpProject::discover(&project_root).unwrap();
    assert_eq!(project.package.name, "test/sample-project");
    assert_eq!(project.package.version, "1.0.0");

    let indexer = Indexer::new(project);
    let index = indexer.index().unwrap();

    // Should have metadata
    assert!(index.metadata.is_some());
    let metadata = index.metadata.as_ref().unwrap();
    assert!(metadata.tool_info.is_some());
    assert_eq!(metadata.tool_info.as_ref().unwrap().name, "scip-php");

    // Should have documents for each PHP file
    assert!(!index.documents.is_empty());

    // Find the User document
    let user_doc = index
        .documents
        .iter()
        .find(|d| d.relative_path.contains("User.php"))
        .expect("Should have a User.php document");

    assert_eq!(user_doc.language, "PHP");

    // Should have occurrences for class, methods, properties
    assert!(!user_doc.occurrences.is_empty());

    // Check that User class definition exists
    let user_def = user_doc
        .occurrences
        .iter()
        .find(|o| {
            o.symbol.contains("User#")
                && !o.symbol.contains("UserRepository")
                && (o.symbol_roles & scip::types::SymbolRole::Definition as i32) != 0
        })
        .expect("Should have User class definition");

    assert!(user_def.symbol.contains("test/sample-project"));
    assert!(user_def.symbol.contains("App/Models/User#"));

    // Check that Status enum document exists
    let status_doc = index
        .documents
        .iter()
        .find(|d| d.relative_path.contains("Status.php"))
        .expect("Should have a Status.php document");

    // Should have enum case definitions
    let active_case = status_doc
        .occurrences
        .iter()
        .find(|o| {
            o.symbol.contains("Active")
                && (o.symbol_roles & scip::types::SymbolRole::Definition as i32) != 0
        })
        .expect("Should have Active enum case definition");

    assert!(active_case.symbol.contains("App/Enums/Status#Active."));

    // Find the UserRepository interface document
    let repo_doc = index
        .documents
        .iter()
        .find(|d| d.relative_path.contains("UserRepository.php"))
        .expect("Should have a UserRepository.php document");

    // Should have interface definition
    let repo_def = repo_doc
        .occurrences
        .iter()
        .find(|o| {
            o.symbol.contains("UserRepository#")
                && (o.symbol_roles & scip::types::SymbolRole::Definition as i32) != 0
        })
        .expect("Should have UserRepository interface definition");

    assert!(repo_def.symbol.contains("App/Contracts/UserRepository#"));
}

#[test]
fn test_scip_output_file() {
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample-project");

    let project = PhpProject::discover(&project_root).unwrap();
    let indexer = Indexer::new(project);
    let index = indexer.index().unwrap();

    // Write to a temp file and verify it can be read back
    let output_path = std::env::temp_dir().join("test-scip-php.scip");
    scip::write_message_to_file(output_path.to_str().unwrap(), index).unwrap();

    assert!(output_path.exists());
    let file_size = std::fs::metadata(&output_path).unwrap().len();
    assert!(file_size > 0, "SCIP file should not be empty");

    // Clean up
    let _ = std::fs::remove_file(&output_path);
}

#[test]
fn test_user_class_members() {
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample-project");

    let project = PhpProject::discover(&project_root).unwrap();
    let indexer = Indexer::new(project);
    let index = indexer.index().unwrap();

    let user_doc = index
        .documents
        .iter()
        .find(|d| d.relative_path.contains("User.php"))
        .expect("Should have User.php");

    // Check method definitions
    let has_get_name = user_doc.occurrences.iter().any(|o| {
        o.symbol.contains("getName().")
            && (o.symbol_roles & scip::types::SymbolRole::Definition as i32) != 0
    });
    assert!(has_get_name, "Should have getName method definition");

    let has_set_name = user_doc.occurrences.iter().any(|o| {
        o.symbol.contains("setName().")
            && (o.symbol_roles & scip::types::SymbolRole::Definition as i32) != 0
    });
    assert!(has_set_name, "Should have setName method definition");

    let has_constructor = user_doc.occurrences.iter().any(|o| {
        o.symbol.contains("__construct().")
            && (o.symbol_roles & scip::types::SymbolRole::Definition as i32) != 0
    });
    assert!(has_constructor, "Should have __construct method definition");

    // Check property definitions
    let has_name_prop = user_doc.occurrences.iter().any(|o| {
        o.symbol.contains("User#name.")
            && (o.symbol_roles & scip::types::SymbolRole::Definition as i32) != 0
    });
    assert!(has_name_prop, "Should have $name property definition");

    let has_age_prop = user_doc.occurrences.iter().any(|o| {
        o.symbol.contains("User#age.")
            && (o.symbol_roles & scip::types::SymbolRole::Definition as i32) != 0
    });
    assert!(has_age_prop, "Should have $age property definition");

    // Check class constant
    let has_max_name = user_doc.occurrences.iter().any(|o| {
        o.symbol.contains("User#MAX_NAME_LENGTH.")
            && (o.symbol_roles & scip::types::SymbolRole::Definition as i32) != 0
    });
    assert!(
        has_max_name,
        "Should have MAX_NAME_LENGTH constant definition"
    );

    // Check constructor symbol info is marked as Constructor kind
    let constructor_sym = user_doc
        .symbols
        .iter()
        .find(|s| s.symbol.contains("__construct()."))
        .expect("Should have __construct SymbolInformation");
    assert_eq!(
        constructor_sym.kind,
        scip::types::symbol_information::Kind::Constructor.into()
    );
}

#[test]
fn test_enum_cases() {
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample-project");

    let project = PhpProject::discover(&project_root).unwrap();
    let indexer = Indexer::new(project);
    let index = indexer.index().unwrap();

    let status_doc = index
        .documents
        .iter()
        .find(|d| d.relative_path.contains("Status.php"))
        .expect("Should have Status.php");

    // Check all three enum cases
    for case_name in &["Active", "Inactive", "Pending"] {
        let has_case = status_doc.occurrences.iter().any(|o| {
            o.symbol.contains(&format!("Status#{case_name}."))
                && (o.symbol_roles & scip::types::SymbolRole::Definition as i32) != 0
        });
        assert!(has_case, "Should have {case_name} enum case definition");
    }

    // Check enum definition itself
    let enum_def = status_doc.occurrences.iter().any(|o| {
        o.symbol.ends_with("Status#")
            && (o.symbol_roles & scip::types::SymbolRole::Definition as i32) != 0
    });
    assert!(enum_def, "Should have Status enum definition");
}

#[test]
fn test_expression_references() {
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample-project");

    let project = PhpProject::discover(&project_root).unwrap();
    let indexer = Indexer::new(project);
    let index = indexer.index().unwrap();

    let service_doc = index
        .documents
        .iter()
        .find(|d| d.relative_path.contains("UserService.php"))
        .expect("Should have UserService.php");

    // Definitions: UserService class, createUser method, getUserName method, isActive method
    let has_service_def = service_doc.occurrences.iter().any(|o| {
        o.symbol.contains("UserService#")
            && (o.symbol_roles & scip::types::SymbolRole::Definition as i32) != 0
    });
    assert!(has_service_def, "Should have UserService class definition");

    // Check that the `use App\Models\User` import emits a reference to User
    let has_use_user_ref = service_doc.occurrences.iter().any(|o| {
        o.symbol.contains("App/Models/User#")
            && (o.symbol_roles & scip::types::SymbolRole::Definition as i32) == 0
    });
    assert!(
        has_use_user_ref,
        "Should have a reference to User from use statement or type hint"
    );

    // Check that "new User(...)" emits a reference to User
    // The Instantiation expression should produce a reference occurrence for User
    let user_refs: Vec<_> = service_doc
        .occurrences
        .iter()
        .filter(|o| {
            o.symbol.contains("App/Models/User#")
                && (o.symbol_roles & scip::types::SymbolRole::Definition as i32) == 0
        })
        .collect();
    // At minimum: use statement, return type hint on createUser, param type on getUserName, and `new User()`
    assert!(
        user_refs.len() >= 3,
        "Should have at least 3 references to User, found {}",
        user_refs.len()
    );

    // Check that Status is referenced (from use statement, param type, and Status::Active)
    let status_refs: Vec<_> = service_doc
        .occurrences
        .iter()
        .filter(|o| {
            o.symbol.contains("App/Enums/Status#")
                && !o.symbol.contains("Active")
                && (o.symbol_roles & scip::types::SymbolRole::Definition as i32) == 0
        })
        .collect();
    assert!(
        status_refs.len() >= 2,
        "Should have at least 2 references to Status, found {}",
        status_refs.len()
    );
}

#[test]
fn test_phpdoc_extraction() {
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample-project");

    let project = PhpProject::discover(&project_root).unwrap();
    let indexer = Indexer::new(project);
    let index = indexer.index().unwrap();

    let user_doc = index
        .documents
        .iter()
        .find(|d| d.relative_path.contains("User.php"))
        .expect("Should have User.php");

    // Check that User class has documentation from PHPDoc
    let user_sym = user_doc
        .symbols
        .iter()
        .find(|s| s.symbol.contains("User#") && !s.symbol.contains("UserRepository"))
        .expect("Should have User SymbolInformation");

    assert!(
        !user_sym.documentation.is_empty(),
        "User class should have documentation from PHPDoc"
    );
    assert!(
        user_sym
            .documentation
            .iter()
            .any(|d| d.contains("Represents a user")),
        "User documentation should contain description text, got: {:?}",
        user_sym.documentation
    );

    // Check that __construct has documentation
    let constructor_sym = user_doc
        .symbols
        .iter()
        .find(|s| s.symbol.contains("__construct()."))
        .expect("Should have __construct SymbolInformation");

    assert!(
        !constructor_sym.documentation.is_empty(),
        "__construct should have documentation from PHPDoc"
    );
    assert!(
        constructor_sym
            .documentation
            .iter()
            .any(|d| d.contains("Create a new User instance")),
        "__construct documentation should contain description, got: {:?}",
        constructor_sym.documentation
    );

    // Check that getName has documentation
    let get_name_sym = user_doc
        .symbols
        .iter()
        .find(|s| s.symbol.contains("getName()."))
        .expect("Should have getName SymbolInformation");

    assert!(
        !get_name_sym.documentation.is_empty(),
        "getName should have documentation from PHPDoc"
    );
    assert!(
        get_name_sym
            .documentation
            .iter()
            .any(|d| d.contains("Get the user's name")),
        "getName documentation should contain description, got: {:?}",
        get_name_sym.documentation
    );

    // setName has no PHPDoc, so its documentation should be empty
    let set_name_sym = user_doc
        .symbols
        .iter()
        .find(|s| s.symbol.contains("setName()."))
        .expect("Should have setName SymbolInformation");

    assert!(
        set_name_sym.documentation.is_empty(),
        "setName should have no documentation (no PHPDoc), got: {:?}",
        set_name_sym.documentation
    );
}
