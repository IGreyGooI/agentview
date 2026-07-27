use agentview::pom::{
    BlockChildren, BlockContent, Document, InlineChildren, InlineContent, ListItem, ListKind,
    ListNode, MarkdownNode, StrongNode, TextNode, XmlName, XmlNode,
};
use agentview::pom_renderer::{render_pom_document, PomRenderError};
use agentview::pom_resolution::resolve_system_document;

#[test]
fn renders_canonical_markdown_and_xml_system_document() {
    let mut response_contract = XmlNode::try_build("response_contract", |children| {
        children.text(TextNode::new("Return <select> & "));

        let mut strong = InlineChildren::new();
        strong.push(InlineContent::try_text("nothing else").unwrap());
        children.markdown(MarkdownNode::Strong(StrongNode::new(strong)));

        children.text(TextNode::new("."));
        Ok(())
    })
    .unwrap();
    response_contract
        .push_attribute(XmlName::try_from("transport").unwrap(), "xml & strict")
        .unwrap();

    let document = Document::try_build(|blocks| {
        blocks.try_heading(1, |heading| {
            heading.try_text("Demo <selector>")?;
            Ok(())
        })?;
        blocks.try_paragraph(|paragraph| {
            paragraph.try_text("Choose *one* ")?;
            paragraph.try_strong(|strong| {
                strong.try_text("grounded")?;
                Ok(())
            })?;
            paragraph.try_text(" action with ")?;
            paragraph.code_span(TextNode::new(r#"<select local_id="..."/>"#));
            paragraph.try_text(".")?;
            Ok(())
        })?;
        blocks.try_list(ListKind::Ordered { start: 2 }, |list| {
            list.try_item(|item| {
                item.try_paragraph(|paragraph| {
                    paragraph.try_text("Verify intent.")?;
                    Ok(())
                })?;
                Ok(())
            })?;
            list.try_item(|item| {
                item.try_paragraph(|paragraph| {
                    paragraph.try_text("Return result.")?;
                    Ok(())
                })?;
                Ok(())
            })?;
            Ok(())
        })?;
        blocks.code_block(
            Some("xml".into()),
            TextNode::new(r#"<select local_id="x"/>"#),
        );
        blocks.thematic_break();
        blocks.xml(response_contract);
        Ok(())
    })
    .unwrap();

    let rendered = render_pom_document(&resolve_system_document(document)).unwrap();

    assert_eq!(
        rendered,
        concat!(
            "# Demo \\<selector\\>\n\n",
            "Choose \\*one\\* **grounded** action with `<select local_id=\"...\"/>`.\n\n",
            "2. Verify intent.\n",
            "3. Return result.\n\n",
            "~~~ xml\n",
            "<select local_id=\"x\"/>\n",
            "~~~\n\n",
            "---\n\n",
            "<response_contract transport=\"xml &amp; strict\">",
            "Return &lt;select&gt; &amp; **nothing else**.",
            "</response_contract>"
        )
    );
}

#[test]
fn renderer_pretty_prints_block_element_only_xml_without_expanding_leaves() {
    let context = XmlNode::try_build("agent_context", |children| {
        children.xml(XmlNode::try_build("phase", |phase| {
            phase.text(TextNode::new("execute"));
            Ok(())
        })?);
        children.xml(XmlNode::try_build("focus", |focus| {
            focus.xml(XmlNode::try_build("summary", |summary| {
                summary.text(TextNode::new("Attach the verified edge."));
                Ok(())
            })?);
            focus.xml(XmlNode::try_build("rationale", |rationale| {
                rationale.xml(XmlNode::try_build("none", |_| Ok(()))?);
                Ok(())
            })?);
            Ok(())
        })?);
        Ok(())
    })
    .unwrap();

    let document = Document::build(|blocks| blocks.xml(context));

    assert_eq!(
        render_pom_document(&resolve_system_document(document)).unwrap(),
        concat!(
            "<agent_context>\n",
            "  <phase>execute</phase>\n",
            "  <focus>\n",
            "    <summary>Attach the verified edge.</summary>\n",
            "    <rationale>\n",
            "      <none />\n",
            "    </rationale>\n",
            "  </focus>\n",
            "</agent_context>",
        )
    );
}

#[test]
fn renderer_keeps_inline_and_mixed_xml_compact() {
    let tool = XmlNode::try_build("tool", |children| {
        children.xml(XmlNode::try_build("purpose", |purpose| {
            purpose.text(TextNode::new("Inspect one edge."));
            Ok(())
        })?);
        Ok(())
    })
    .unwrap();
    let mixed = XmlNode::try_build("message", |children| {
        children.text(TextNode::new("before "));
        children.xml(XmlNode::try_build("emphasis", |emphasis| {
            emphasis.text(TextNode::new("middle"));
            Ok(())
        })?);
        children.text(TextNode::new(" after"));
        Ok(())
    })
    .unwrap();
    let document = Document::try_build(|blocks| {
        blocks.try_paragraph(|paragraph| {
            paragraph.try_text("Call ")?;
            paragraph.xml(tool);
            paragraph.try_text(" now.")
        })?;
        blocks.xml(mixed);
        Ok(())
    })
    .unwrap();

    assert_eq!(
        render_pom_document(&resolve_system_document(document)).unwrap(),
        concat!(
            "Call <tool><purpose>Inspect one edge.</purpose></tool> now.\n\n",
            "<message>before <emphasis>middle</emphasis> after</message>",
        )
    );
}

#[test]
fn renderer_does_not_indent_nested_block_markdown_as_xml_layout() {
    let child = XmlNode::try_build("child", |children| {
        let mut paragraph = InlineChildren::new();
        paragraph.push(InlineContent::try_text("Markdown block").unwrap());
        children.markdown(MarkdownNode::Paragraph(agentview::pom::ParagraphNode::new(
            paragraph,
        )));
        Ok(())
    })
    .unwrap();
    let outer = XmlNode::try_build("outer", |children| {
        children.xml(child);
        Ok(())
    })
    .unwrap();
    let document = Document::build(|blocks| blocks.xml(outer));

    assert_eq!(
        render_pom_document(&resolve_system_document(document)).unwrap(),
        "<outer>\n<child>\nMarkdown block\n</child>\n</outer>",
    );
}

#[test]
fn renderer_combines_xml_layout_with_markdown_list_indentation_only_when_safe() {
    fn nested_xml() -> XmlNode {
        XmlNode::try_build("root", |children| {
            children.xml(XmlNode::try_build("leaf", |leaf| {
                leaf.text(TextNode::new("value"));
                Ok(())
            })?);
            Ok(())
        })
        .unwrap()
    }

    let xml_with_list = XmlNode::try_build("outer", |children| {
        let mut item_children = BlockChildren::new();
        item_children.push(BlockContent::xml(nested_xml()));
        let list = ListNode::new(ListKind::Unordered, vec![ListItem::new(item_children)]);
        children.markdown(MarkdownNode::List(list));
        Ok(())
    })
    .unwrap();
    let document = Document::try_build(|blocks| {
        blocks.try_list(ListKind::Unordered, |items| {
            items.try_item(|item| {
                item.xml(nested_xml());
                Ok(())
            })?;
            Ok(())
        })?;
        blocks.xml(xml_with_list);
        Ok(())
    })
    .unwrap();

    assert_eq!(
        render_pom_document(&resolve_system_document(document)).unwrap(),
        concat!(
            "- <root>\n",
            "    <leaf>value</leaf>\n",
            "  </root>\n\n",
            "<outer>\n",
            "- <root><leaf>value</leaf></root>\n",
            "</outer>",
        )
    );
}

#[test]
fn renderer_keeps_attributes_and_authored_multiline_text_on_one_xml_line() {
    let mut message = XmlNode::try_build("message", |children| {
        children.text(TextNode::new("line one\nline two"));
        Ok(())
    })
    .unwrap();
    message
        .push_attribute(
            XmlName::try_from("note").unwrap(),
            "attribute one\nattribute two",
        )
        .unwrap();
    let document = Document::build(|blocks| blocks.xml(message));

    assert_eq!(
        render_pom_document(&resolve_system_document(document)).unwrap(),
        concat!(
            "<message note=\"attribute one&#10;attribute two\">",
            "line one&#10;line two",
            "</message>",
        )
    );
}

#[test]
fn renderer_uses_dynamic_fences_code_span_padding_and_multiblock_list_indent() {
    let document = Document::try_build(|blocks| {
        blocks.try_list(ListKind::Unordered, |list| {
            list.try_item(|item| {
                item.try_paragraph(|paragraph| {
                    paragraph.try_text("first ")?;
                    paragraph.code_span(TextNode::new("`edge`"));
                    Ok(())
                })?;
                item.code_block(None, TextNode::new("~~~\ninside"));
                Ok(())
            })?;
            Ok(())
        })?;
        Ok(())
    })
    .unwrap();

    let rendered = render_pom_document(&resolve_system_document(document)).unwrap();

    assert_eq!(
        rendered,
        concat!(
            "- first `` `edge` ``\n",
            "\n",
            "  ~~~~\n",
            "  ~~~\n",
            "  inside\n",
            "  ~~~~"
        )
    );
}

#[test]
fn renderer_separates_xml_flow_and_block_runs_without_indenting_markdown() {
    let mut child = XmlNode::new(XmlName::try_from("child").unwrap());
    child
        .push_attribute(XmlName::try_from("data").unwrap(), "\"'\t\n\r&<>")
        .unwrap();

    let section = XmlNode::try_build("section", |children| {
        children.text(TextNode::new("prefix & "));

        let mut strong = InlineChildren::new();
        strong.push(InlineContent::try_text("*literal* <x>").unwrap());
        children.markdown(MarkdownNode::Strong(StrongNode::new(strong)));

        let mut paragraph = InlineChildren::new();
        paragraph.push(InlineContent::try_text("block <text>").unwrap());
        children.markdown(MarkdownNode::Paragraph(agentview::pom::ParagraphNode::new(
            paragraph,
        )));

        children.xml(child);
        children.text(TextNode::new(" suffix"));
        Ok(())
    })
    .unwrap();

    let document = Document::build(|blocks| blocks.xml(section));
    let rendered = render_pom_document(&resolve_system_document(document)).unwrap();

    assert_eq!(
        rendered,
        concat!(
            "<section>\n",
            "prefix &amp; **\\*literal\\* &lt;x&gt;**\n\n",
            "block &lt;text&gt;\n\n",
            "<child data=\"&quot;&apos;&#9;&#10;&#13;&amp;&lt;&gt;\" /> suffix\n",
            "</section>"
        )
    );
}

#[test]
fn renderer_rejects_invalid_code_language_and_xml_control_characters() {
    let invalid_language = Document::build(|blocks| {
        blocks.code_block(Some("ru\nst".into()), TextNode::new("fn main() {}"));
    });
    assert!(matches!(
        render_pom_document(&resolve_system_document(invalid_language)),
        Err(PomRenderError::InvalidCodeBlockLanguage { .. })
    ));

    let invalid_xml = Document::build(|blocks| {
        blocks.xml(
            XmlNode::try_build("value", |children| {
                children.text(TextNode::new("bad\u{0001}value"));
                Ok(())
            })
            .unwrap(),
        );
    });
    assert_eq!(
        render_pom_document(&resolve_system_document(invalid_xml)),
        Err(PomRenderError::InvalidXmlCharacter { code_point: 1 })
    );
}

#[test]
fn empty_resolved_document_renders_empty_text() {
    let rendered = render_pom_document(&resolve_system_document(Document::build(|_| {}))).unwrap();
    assert!(rendered.is_empty());
}

#[test]
fn renderer_keeps_block_like_paragraph_text_literal() {
    let cases = [
        ("  # injected", "  \\# injected"),
        ("    indented", "&#32;   indented"),
        ("~~~", "\\~~~"),
        ("  1. injected", "  1\\. injected"),
    ];

    for (input, expected) in cases {
        let document = Document::try_build(|blocks| {
            blocks.try_paragraph(|paragraph| {
                paragraph.try_text(input)?;
                Ok(())
            })?;
            Ok(())
        })
        .unwrap();

        assert_eq!(
            render_pom_document(&resolve_system_document(document)).unwrap(),
            expected,
            "input {input:?}"
        );
    }
}

#[test]
fn renderer_distinguishes_plain_xml_text_from_markdown_children() {
    let plain = Document::build(|blocks| {
        blocks.xml(
            XmlNode::try_build("x", |children| {
                children.text(TextNode::new("**same**\r\n\nb"));
                Ok(())
            })
            .unwrap(),
        );
    });
    let strong = Document::build(|blocks| {
        blocks.xml(
            XmlNode::try_build("x", |children| {
                let mut content = InlineChildren::new();
                content.push(InlineContent::try_text("same").unwrap());
                children.markdown(MarkdownNode::Strong(StrongNode::new(content)));
                Ok(())
            })
            .unwrap(),
        );
    });

    let plain = render_pom_document(&resolve_system_document(plain)).unwrap();
    let strong = render_pom_document(&resolve_system_document(strong)).unwrap();

    assert_eq!(plain, "<x>\\*\\*same\\*\\*&#13;&#10;&#10;b</x>");
    assert_eq!(strong, "<x>**same**</x>");
    assert_ne!(plain, strong);
}

#[test]
fn renderer_preserves_strong_boundary_whitespace_and_rejects_empty_strong() {
    let padded = Document::try_build(|blocks| {
        blocks.try_paragraph(|paragraph| {
            paragraph.try_strong(|strong| {
                strong.try_text(" x ")?;
                Ok(())
            })?;
            Ok(())
        })?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        render_pom_document(&resolve_system_document(padded)).unwrap(),
        " **x** "
    );

    let empty = Document::build(|blocks| {
        blocks.paragraph(|paragraph| {
            paragraph.strong(|_| {});
        });
    });
    assert!(render_pom_document(&resolve_system_document(empty)).is_err());

    let whitespace_only = Document::try_build(|blocks| {
        blocks.try_paragraph(|paragraph| {
            paragraph.try_strong(|strong| {
                strong.try_text("   ")?;
                Ok(())
            })?;
            Ok(())
        })?;
        Ok(())
    })
    .unwrap();
    assert!(render_pom_document(&resolve_system_document(whitespace_only)).is_err());

    let indented = Document::try_build(|blocks| {
        blocks.try_paragraph(|paragraph| {
            paragraph.try_strong(|strong| {
                strong.try_text("    x")?;
                Ok(())
            })?;
            Ok(())
        })?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        render_pom_document(&resolve_system_document(indented)).unwrap(),
        "&#32;   **x**"
    );

    let unicode = Document::try_build(|blocks| {
        blocks.try_paragraph(|paragraph| {
            paragraph.try_strong(|strong| {
                strong.try_text("界 ")?;
                Ok(())
            })?;
            Ok(())
        })?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        render_pom_document(&resolve_system_document(unicode)).unwrap(),
        "**界** "
    );
}

#[test]
fn renderer_rejects_unrepresentable_code_spans() {
    for body in ["", "a\nb", "a\rb"] {
        let document = Document::build(|blocks| {
            blocks.paragraph(|paragraph| {
                paragraph.code_span(TextNode::new(body));
            });
        });

        assert!(
            render_pom_document(&resolve_system_document(document)).is_err(),
            "body {body:?}"
        );
    }
}

#[test]
fn renderer_separates_code_fence_from_language_and_rejects_whitespace_languages() {
    let tilde_language = Document::build(|blocks| {
        blocks.code_block(Some("~rust".into()), TextNode::new("body"));
    });
    assert_eq!(
        render_pom_document(&resolve_system_document(tilde_language)).unwrap(),
        "~~~ ~rust\nbody\n~~~"
    );

    for language in ["", " rust", "rust ", "rust test", "ru\nst"] {
        let document = Document::build(|blocks| {
            blocks.code_block(Some(language.into()), TextNode::new("body"));
        });
        assert!(
            render_pom_document(&resolve_system_document(document)).is_err(),
            "language {language:?}"
        );
    }
}

#[test]
fn renderer_preserves_an_empty_code_block_body() {
    let document = Document::build(|blocks| {
        blocks.code_block(None, TextNode::new(""));
    });

    assert_eq!(
        render_pom_document(&resolve_system_document(document)).unwrap(),
        "~~~\n~~~"
    );
}

#[test]
fn renderer_rejects_empty_paragraphs_and_zero_item_lists() {
    let empty_paragraph = Document::build(|blocks| {
        blocks.paragraph(|_| {});
    });
    assert!(render_pom_document(&resolve_system_document(empty_paragraph)).is_err());

    let empty_list = Document::build(|blocks| {
        blocks.list(ListKind::Unordered, |_| {});
    });
    assert!(render_pom_document(&resolve_system_document(empty_list)).is_err());
}

#[test]
fn renderer_preserves_empty_headings_and_list_items() {
    let document = Document::build(|blocks| {
        blocks.heading(agentview::pom::HeadingLevel::try_from(1).unwrap(), |_| {});
        blocks.list(ListKind::Unordered, |list| {
            list.item(|_| {});
            list.item(|_| {});
        });
    });

    assert_eq!(
        render_pom_document(&resolve_system_document(document)).unwrap(),
        "# \n\n-\n-"
    );
}

#[test]
fn renderer_rejects_non_commonmark_ordered_markers() {
    let document = Document::build(|blocks| {
        blocks.list(
            ListKind::Ordered {
                start: 1_000_000_000,
            },
            |list| {
                list.item(|item| {
                    item.paragraph(|paragraph| {
                        paragraph.try_text("value").unwrap();
                    });
                });
            },
        );
    });

    assert!(render_pom_document(&resolve_system_document(document)).is_err());
}

#[test]
fn renderer_reuses_a_valid_ordered_marker_when_display_number_exceeds_nine_digits() {
    let document = Document::build(|blocks| {
        blocks.list(ListKind::Ordered { start: 999_999_999 }, |list| {
            for value in ["first", "second"] {
                list.item(|item| {
                    item.paragraph(|paragraph| {
                        paragraph.try_text(value).unwrap();
                    });
                });
            }
        });
    });

    assert_eq!(
        render_pom_document(&resolve_system_document(document)).unwrap(),
        "999999999. first\n999999999. second"
    );
}

#[test]
fn renderer_keeps_heading_closing_hashes_and_list_thematic_breaks_structural() {
    let heading = Document::try_build(|blocks| {
        blocks.try_heading(1, |content| {
            content.try_text("#")?;
            Ok(())
        })?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        render_pom_document(&resolve_system_document(heading)).unwrap(),
        "# \\#"
    );

    let list = Document::build(|blocks| {
        blocks.list(ListKind::Unordered, |items| {
            items.item(|item| {
                item.thematic_break();
            });
        });
    });
    assert_eq!(
        render_pom_document(&resolve_system_document(list)).unwrap(),
        "-\n  ---"
    );

    let xml = Document::build(|blocks| {
        blocks.xml(
            XmlNode::try_build("x", |children| {
                children.text(TextNode::new("---"));

                let mut paragraph = InlineChildren::new();
                paragraph.push(InlineContent::try_text("block").unwrap());
                children.markdown(MarkdownNode::Paragraph(agentview::pom::ParagraphNode::new(
                    paragraph,
                )));
                Ok(())
            })
            .unwrap(),
        );
    });
    assert_eq!(
        render_pom_document(&resolve_system_document(xml)).unwrap(),
        "<x>\n\\---\n\nblock\n</x>"
    );
}
