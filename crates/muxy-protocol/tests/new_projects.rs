use muxy_protocol::{PROJECT_COLORS, ProjectDescriptor, ServerPath};

#[test]
fn new_projects_take_an_unused_color_until_every_color_is_taken() {
    let mut taken: Vec<String> = Vec::new();
    for index in 0..PROJECT_COLORS.len() * 2 {
        let directory = ServerPath(format!("/home/dev/app{index}/").into_bytes());
        let project = ProjectDescriptor::new(directory, taken.iter().map(String::as_str));
        assert_eq!(project.validate(), Ok(()));
        assert_eq!(project.name, format!("app{index}"));
        assert!(PROJECT_COLORS.contains(&project.color.as_str()));
        if index < PROJECT_COLORS.len() {
            assert!(!taken.contains(&project.color), "{index}");
        }
        taken.push(project.color);
    }
    let home = ProjectDescriptor::home(ServerPath(b"/home/dev".to_vec()));
    assert_eq!(home.validate(), Ok(()));
    assert!(PROJECT_COLORS.contains(&home.color.as_str()));
}

#[test]
fn default_project_colors_are_neutral_grays() -> Result<(), std::num::ParseIntError> {
    for color in PROJECT_COLORS {
        let hex = color.trim_start_matches('#');
        assert_eq!(hex.len(), 6, "{color}");
        let mut channels = Vec::new();
        for index in 0..3 {
            channels.push(u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)?);
        }
        let spread = channels.iter().max().copied().unwrap_or(0)
            - channels.iter().min().copied().unwrap_or(0);
        assert!(spread <= 4, "{color} has a hue");
    }
    Ok(())
}
