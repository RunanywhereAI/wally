package tui

// Header renders title with the theme's accent mark, for the top of every
// full-screen prompt in this package.
func Header(title string) string {
	return titleStyle.Render(bloom+" "+title) + "\n"
}
