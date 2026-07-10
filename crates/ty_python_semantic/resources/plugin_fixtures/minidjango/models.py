from typing import TypedDict

import accounts
import library
import minidjango
import minidjango_settings


class BookManager: ...


class BookValueRow(TypedDict):
    title: str
    pages: int | None


class BookTitleRow(TypedDict):
    title: str


class Book(minidjango.Model):
    title = minidjango.CharField(max_length=200)
    pages = minidjango.IntegerField(null=True)
    author = minidjango.ForeignKey("library.Author", related_name="books")
    alternate_author = minidjango.ForeignKey("library.Author", null=True, related_name="books")
    parent = minidjango.ForeignKey("self", null=True, related_name="children")
    missing_author = minidjango.ForeignKey("library.Missing", null=True, related_name="missing_books")
    owner = minidjango.ForeignKey(minidjango_settings.AUTH_USER_MODEL, null=True, related_name="owned_books")
    published = BookManager()


annotate_method_probe = Book.objects.annotate
annotated_probe = Book.objects.annotate(score=1).get()
annotated_score_probe = annotated_probe.score


def check(a: library.Author, b: Book, u: accounts.User) -> None:
    Book(title="ok", author=a)
    Book(title=123, author=a)
    Book(title="ok", author=b)
    title_from_instance: str = b.title
    bad_title_from_instance: int = b.title
    default_manager: minidjango.Manager[Book] = Book._default_manager
    bad_default_manager: minidjango.Manager[library.Author] = Book._default_manager
    custom_manager: minidjango.Manager[Book] = Book.published
    bad_custom_manager: minidjango.Manager[library.Author] = Book.published
    books: minidjango.QuerySet[Book, Book] = Book.objects.filter(title="ok")
    bad_books: minidjango.QuerySet[library.Author, library.Author] = Book.objects.filter(title="ok")
    published_books: minidjango.QuerySet[Book, Book] = Book.published.filter(title="ok")
    bad_published_books: minidjango.QuerySet[library.Author, library.Author] = Book.published.filter(title="ok")
    chained_books: minidjango.QuerySet[Book, Book] = Book.objects.filter(title="ok").filter(pages=1)
    bad_chained_books: minidjango.QuerySet[library.Author, library.Author] = Book.objects.filter(title="ok").filter(pages=1)
    exact_books: minidjango.QuerySet[Book, Book] = Book.objects.filter(title__exact="ok")
    iexact_books: minidjango.QuerySet[Book, Book] = Book.objects.filter(title__iexact="ok")
    contains_books: minidjango.QuerySet[Book, Book] = Book.objects.filter(title__contains="ok")
    regex_books: minidjango.QuerySet[Book, Book] = Book.objects.filter(title__regex="^ok")
    iregex_books: minidjango.QuerySet[Book, Book] = Book.objects.filter(title__iregex="^ok")
    nullable_books: minidjango.QuerySet[Book, Book] = Book.objects.filter(pages__isnull=True)
    range_books: minidjango.QuerySet[Book, Book] = Book.objects.filter(pages__range=(1, 10))
    author_named_books: minidjango.QuerySet[Book, Book] = Book.objects.filter(author__name="Ada")
    owner_named_books: minidjango.QuerySet[Book, Book] = Book.objects.filter(owner__username="kev")
    Book.objects.filter(pages__isnull="yes")
    Book.objects.filter(pages__gt="many")
    Book.objects.filter(title__contains=1)
    Book.objects.filter(pages__range=(1, "many"))
    Book.objects.filter(missing="bad")
    Book.objects.get(title__year="bad")
    Book.objects.filter(author__missing="bad")
    value_rows: minidjango.QuerySet[Book, BookValueRow] = Book.objects.values("title", "pages")
    bad_value_rows: minidjango.QuerySet[Book, dict[str, int]] = Book.objects.values("title", "pages")
    chained_value_rows: minidjango.QuerySet[Book, BookTitleRow] = Book.objects.values("title").filter(author__name="Ada")
    bad_chained_value_rows: minidjango.QuerySet[Book, dict[str, int]] = Book.objects.values("title").filter(author__name="Ada")
    Book.objects.values("missing")
    title_rows: minidjango.QuerySet[Book, str] = Book.objects.values_list("title", flat=True)
    bad_title_rows: minidjango.QuerySet[Book, int] = Book.objects.values_list("title", flat=True)
    page_rows: minidjango.QuerySet[Book, int | None] = Book.objects.values_list("pages", flat=True)
    bad_page_rows: minidjango.QuerySet[Book, str] = Book.objects.values_list("pages", flat=True)
    title_page_rows: minidjango.QuerySet[Book, tuple[str, int | None]] = Book.objects.values_list("title", "pages")
    bad_title_page_rows: minidjango.QuerySet[Book, tuple[int, str]] = Book.objects.values_list("title", "pages")
    chained_title_rows: minidjango.QuerySet[Book, str] = Book.objects.values_list("title", flat=True).filter(pages=1)
    bad_chained_title_rows: minidjango.QuerySet[Book, int] = Book.objects.values_list("title", flat=True).filter(pages=1)
    chained_title_page_rows: minidjango.QuerySet[Book, tuple[str, int | None]] = Book.objects.values_list("title", "pages").filter(title="ok")
    bad_chained_title_page_rows: minidjango.QuerySet[Book, tuple[int, str]] = Book.objects.values_list("title", "pages").filter(title="ok")
    named_title_value: str = Book.objects.values_list("title", named=True).get().title
    bad_named_title_value: int = Book.objects.values_list("title", named=True).get().title
    chained_named_page_value: int | None = Book.objects.values_list("title", "pages", named=True).filter(title="ok").get().pages
    bad_chained_named_page_value: str = Book.objects.values_list("title", "pages", named=True).filter(title="ok").get().pages
    Book.objects.values_list("missing", flat=True)
    book: Book = Book.objects.get(title="ok")
    bad_author: library.Author = Book.objects.get(title="ok")
    created: tuple[Book, bool] = Book.objects.get_or_create(title="ok")
    bad_created: tuple[library.Author, bool] = Book.objects.get_or_create(title="ok")
    default_book: Book = Book._default_manager.get(title="ok")
    bad_default_author: library.Author = Book._default_manager.get(title="ok")
    queryset_book: Book = Book.objects.filter(title="ok").get(pages=1)
    bad_queryset_author: library.Author = Book.objects.filter(title="ok").get(pages=1)
    queryset_created: tuple[Book, bool] = Book.objects.filter(title="ok").get_or_create(pages=1)
    bad_queryset_created: tuple[library.Author, bool] = Book.objects.filter(title="ok").get_or_create(pages=1)
    maybe_book: Book | None = Book.objects.first()
    title_value: str = Book.objects.values_list("title", flat=True).get()
    bad_title_value: int = Book.objects.values_list("title", flat=True).get()
    maybe_title: str | None = Book.objects.values_list("title", flat=True).first()
    bad_maybe_title: int = Book.objects.values_list("title", flat=True).first()
    title_page_value: tuple[str, int | None] = Book.objects.values_list("title", "pages").get()
    bad_title_page_value: tuple[str, int] = Book.objects.values_list("title", "pages").get()
    maybe_title_page: tuple[str, int | None] | None = Book.objects.values_list("title", "pages").first()
    bad_maybe_title_page: tuple[str, int] = Book.objects.values_list("title", "pages").first()
    value_row: BookTitleRow = Book.objects.values("title").get()
    bad_value_row: dict[str, int] = Book.objects.values("title").get()
    maybe_value_row: BookTitleRow | None = Book.objects.values("title").first()
    bad_maybe_value_row: dict[str, int] = Book.objects.values("title").first()
    all_value_title: str = Book.objects.values().get()["title"]
    bad_all_value_title: int = Book.objects.values().get()["title"]
    all_named_title: str = Book.objects.values_list(named=True).get().title
    bad_all_named_title: int = Book.objects.values_list(named=True).get().title
    queryset_count: int = Book.objects.filter(title="ok").count()
    bad_queryset_count: str = Book.objects.filter(title="ok").count()
    queryset_exists: bool = Book.objects.filter(title="ok").exists()
    bad_queryset_exists: str = Book.objects.filter(title="ok").exists()
    annotated_book: Book = Book.objects.annotate(score=1).get()
    bad_annotated_author: library.Author = Book.objects.annotate(score=1).get()
    annotated_score: int = Book.objects.annotate(score=1).get().score
    bad_annotated_score: str = Book.objects.annotate(score=1).get().score
    chained_annotated_score: int = Book.objects.filter(title="ok").annotate(score=1).filter(pages=1).get().score
    bad_chained_annotated_score: str = Book.objects.filter(title="ok").annotate(score=1).filter(pages=1).get().score
    books_from_author: minidjango.Manager[Book] = a.books
    bad_reverse: minidjango.Manager[library.Author] = a.books
    children: minidjango.Manager[Book] = b.children
    bad_children: minidjango.Manager[library.Author] = b.children
    owned_books: minidjango.Manager[Book] = u.owned_books
    bad_owned_books: minidjango.Manager[library.Author] = u.owned_books
