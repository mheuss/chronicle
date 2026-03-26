# Accessibility

applies: UI components, user-facing output, frontend changes

## Perceivable

applies: all

### Images and Media
- [ ] All images have meaningful alt text (or empty alt for decorative images)
- [ ] Video content has captions
- [ ] Audio content has transcripts where applicable

### Text and Color
- [ ] Color contrast meets WCAG AA ratio (4.5:1 for normal text, 3:1 for large)
- [ ] Information is not conveyed by color alone
- [ ] Text can be resized to 200% without loss of content

## Operable

applies: all

### Keyboard Navigation
- [ ] All interactive elements are keyboard accessible
- [ ] Focus order is logical and predictable
- [ ] Focus indicators are visible
- [ ] No keyboard traps — user can always navigate away

### Navigation
- [ ] Page has a descriptive title
- [ ] Links have descriptive text (not "click here")
- [ ] Skip navigation link is provided for repeated content

## Understandable

applies: all

### Forms
- [ ] Form fields have visible labels (not just placeholders)
- [ ] Error messages identify the field and describe the error
- [ ] Required fields are clearly indicated
- [ ] Form submission errors don't clear user input

### Language
- [ ] Page language is set in HTML lang attribute
- [ ] Instructions don't rely solely on sensory characteristics (shape, size, position)

## Robust

applies: all

### Markup
- [ ] HTML is valid and well-structured
- [ ] ARIA roles and attributes are used correctly
- [ ] Custom components expose name, role, and state to assistive technology
